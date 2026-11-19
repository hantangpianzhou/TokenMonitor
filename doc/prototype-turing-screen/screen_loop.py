#!/usr/bin/env python3
"""图灵智显 3.5 寸常驻推送程序 —— exe 打包入口。

读 TokenMonitor 的 SQLite → 渲染整帧 → 串口推送到屏。常驻循环，数据驱动 + 时钟兜底。

调度设计（借鉴 turing-smart-screen-python / rafelton / sammcj 三个开源项目）：
  1. 启动立即推一帧（首次快速响应，不等第一个轮询周期）
  2. 定时轮询读库（默认 60s，与 Rust 的 5 分钟扫描解耦）
  3. 数据指纹变化才推；无变化时按 --clock-refresh 周期兜底刷新（保证屏上时钟不过时）
  4. 串口断链重试 N 次，全失败保留上一帧、本轮跳过，下轮再来（LCD 保持特性）
  5. stale 检测：最近活动时间超过阈值时日志告警，不伪装实时
  6. 手动刷新：检测触发文件（默认 %TEMP%/tm-screen-refresh.trigger），存在即推一帧并清除

用法：
    python screen_loop.py --demo --dry-run --once    # 无库无硬件，自测一帧
    python screen_loop.py --list                      # 列出串口，找 1a86:5722
    python screen_loop.py --port COM3                 # 常驻推送（自动探测可省略 --port）
    python screen_loop.py --interval 60 --orient landscape
"""
from __future__ import annotations

import argparse
import os
import sys
import time
from datetime import datetime, timedelta, timezone

# 同目录模块（打包时由 PyInstaller 一并打入）
from render_screen import load_from_db, demo_data, render, to_rgb565_le, fmt_wan
from send_to_screen import Turing35RevA, Orientation, list_ports

try:
    import serial
except ImportError:
    serial = None

TZ8 = timezone(timedelta(hours=8))
DEFAULT_DB = os.path.expanduser("~/AppData/Roaming/TokenMonitor/tokenmonitor.db")
DEFAULT_TRIGGER = os.path.join(os.environ.get("TEMP", "/tmp"), "tm-screen-refresh.trigger")
TURING_VID_PID = (0x1A86, 0x5722)
# 官方 InitializeComm() 先发 6 个 HELLO(69) 并读 6 字节应答，用于识别
# UsbMonitor 子版本；图灵原装 3.5 不应答也属正常，读不到不视为失败。
HELLO_CMD = 69


def setup_console_utf8() -> None:
    """exe 双击运行时控制台默认 GBK，中文日志会乱码；切到 UTF-8。"""
    try:
        import ctypes
        ctypes.windll.kernel32.SetConsoleOutputCP(65001)
        sys.stdout.reconfigure(encoding="utf-8", errors="replace")
    except (AttributeError, OSError):
        pass  # 非 Windows 或无控制台时忽略


def log(msg: str) -> None:
    ts = datetime.now(TZ8).strftime("%H:%M:%S")
    print(f"[{ts}] {msg}", flush=True)


def auto_port() -> str | None:
    """自动探测图灵 3.5 寸串口；三级兜底，兼容新固件换芯片方案的情况。

    老屏（沁恒 CH552）VID:PID = 1a86:5722；新版用微软通用 CDC 驱动
    （usbser.sys 按 USB 设备类匹配），VID:PID 不再是这个值。所以不能死磕
    硬件 ID，按如下优先级找口：
      1. VID:PID 精确命中老屏；
      2. 串口描述/制造商命中关键词（USB Serial / USB 串行端口 / CDC / 图灵…），
         且只有一个候选；
      3. 系统里只有一个串口时直接用它（新屏通常就是唯一 CDC 口）。
    """
    if list_ports is None:
        return None
    ports = list(list_ports.comports())
    # 1) 老屏精确匹配
    for p in ports:
        if p.vid == TURING_VID_PID[0] and p.pid == TURING_VID_PID[1]:
            return p.device

    # 2) 描述/制造商关键词兜底（新屏 CDC 通用驱动）
    keywords = ("usb serial", "usb 串行", "串行端口", "cdc", "ch34", "ch55",
                "turing", "图灵", "usb-serial", "serial port")
    def _blob(p) -> str:
        return " ".join(str(x or "") for x in
                        (p.device, p.description, p.manufacturer)).lower()
    cands = [p for p in ports if any(k in _blob(p) for k in keywords)]
    if len(cands) == 1:
        return cands[0].device

    # 3) 只有一个串口时直接用（新屏在干净机器上常是唯一 CDC 口）
    if len(ports) == 1:
        return ports[0].device
    return None


def data_fingerprint(data: dict) -> tuple:
    """只含 token 数据，不含时钟——避免时钟每分钟变化导致无意义重推。"""
    return (
        data.get("tokens", 0),
        data.get("records", 0),
        data.get("last_activity", "--:--"),
        tuple((n, v) for n, v in data.get("providers", [])),
    )


def load_data(args) -> dict:
    if args.demo:
        data = demo_data()
        # demo 模式注入真实时钟，便于无硬件自测时观察时钟行为
        now = datetime.now(TZ8).strftime("%H:%M")
        data["env"]["clock"] = now
        data["updated"] = now
        return data
    return load_from_db(args.db)


def stale_minutes(data: dict) -> float | None:
    """最近活动时间距今的分钟数；解析失败或 demo 返回 None。"""
    s = data.get("last_activity", "--:--")
    if s == "--:--" or ":" not in s:
        return None
    try:
        now = datetime.now(TZ8)
        t = datetime.strptime(s, "%H:%M").replace(
            year=now.year, month=now.month, day=now.day, tzinfo=TZ8)
        return (now - t).total_seconds() / 60
    except ValueError:
        return None


def build_physical_frame(data: dict, args, dev: Turing35RevA) -> bytes:
    """渲染 → RGB565 → 方向重排 → 物理坐标帧（320列×480行，未包协议头）。

    这是局部刷新比较的最小单位：两帧物理字节直接按块比较即可得到脏矩形，
    位图头坐标也正好是这套物理坐标，无需任何换算。
    """
    img = render(data, portrait=(args.orient == "portrait"))
    # 实机方向校正：物理面板竖装，横屏画面需先转 180° 再做 320 列重排，
    # 否则屏幕上下/左右颠倒（2026-11-19 用户实机确认）。
    if args.orient == "landscape":
        img = img.rotate(180)
    frame = to_rgb565_le(img)
    if args.orient == "landscape":
        frame = dev.rotate_cw(frame)
    return frame


def build_stream_for(data: dict, args, dev: Turing35RevA) -> bytes:
    """渲染 → RGB565 → 按方向重排 → 完整协议字节流。纯内存，不落盘。"""
    frame = build_physical_frame(data, args, dev)
    orientation = Orientation.LANDSCAPE if args.orient == "landscape" else Orientation.PORTRAIT
    return dev.build_stream(frame, orientation, args.brightness,
                            bitmap_cmd=args.bitmap_cmd)


def send_stream(port: str, stream: bytes, args) -> bool:
    """断链重试：全失败返回 False（保留上一帧），由调用方决定下轮再试。

    ★ 本机实测确认的根因（2026-11-19，原装 USB35INCHIPSV2 / COM3）：
    屏幕在串口被打开（DTR 拉高）或收到 RESET(101) 时会**自我复位**，USB 重新
    枚举、COM 口短暂消失，原串口句柄随即失效（读Attribute抛 PermissionError13）。
    所有字节若在这几秒窗口内发出，都进了 OS 缓冲被屏幕端丢弃 → 永远停在
    "please run the app"。唯一验证成功、能点亮的序列是：

        打开 → 发 RESET → 关闭 → 等重枚举 → 重新打开 → 等稳定 → 推帧

    RTS/CTS 硬件流控（rtscts=True）实测反而点不亮，默认关闭。
    """
    if serial is None:
        log("缺少 pyserial：pip install pyserial（打包时已内置则不会遇到）")
        return False

    def open_ser():
        return serial.Serial(
            port,
            baudrate=args.baud,
            timeout=args.serial_timeout,
            rtscts=not args.no_rtscts,
        )

    RESET_PKT = bytes([0, 0, 0, 0, 0, 101])   # command_packet(RESET,0,0,0,0)
    SCREEN_ON_PKT = bytes([0, 0, 0, 0, 0, 109])

    for attempt in range(1, args.retries + 1):
        ser = None
        try:
            if args.open_delay > 0:
                log(f"打开串口前等待 {args.open_delay}s（留出上次关闭后的复位窗口）")
                time.sleep(args.open_delay)

            # === 阶段1：首次打开 + RESET，主动触发屏幕自复位 ===
            if not args.no_reset:
                try:
                    ser = open_ser()
                    # ★ 打开后先等 1s：屏幕在 DTR 拉高瞬间会自复位，立刻发 RESET
                    #   会让两个复位窗口叠加，屏幕卡死在 please run the app。
                    time.sleep(1.0)
                    log(f"首次打开 {port}，发 RESET 触发屏幕自复位: {RESET_PKT.hex()}")
                    ser.write(RESET_PKT)
                    ser.flush()
                    # ★ RESET 后等 2s 让屏幕完成复位（成功脚本实测值）
                    time.sleep(2.0)
                except Exception as re:  # noqa: BLE001 - RESET 写入失败也继续走重枚举
                    log(f"RESET 阶段异常（忽略，仍走重枚举）: {re}")
                try:
                    if ser is not None:
                        ser.close()
                except Exception:
                    pass
                ser = None
                log(f"关闭串口，等待 USB 重新枚举 {args.reenum_wait}s ...")
                time.sleep(args.reenum_wait)

                # === 阶段2：端口重枚举后重新打开（带重试，权威库同样重试 10 次） ===
                for ri in range(1, 11):
                    try:
                        ser = open_ser()
                        log(f"重新打开串口成功（第 {ri} 次）")
                        # ★ 清空复位期间残留的缓冲，成功脚本有这一步
                        ser.reset_input_buffer()
                        ser.reset_output_buffer()
                        break
                    except Exception as ropen:
                        log(f"重开第 {ri}/10 次失败: {ropen}")
                        time.sleep(1)
                if ser is None:
                    raise RuntimeError("重枚举后串口无法重新打开")
            else:
                ser = open_ser()

            log(f"串口已打开 {port} @{args.baud} "
                f"rtscts={'off' if args.no_rtscts else 'on'} "
                f"dtr={ser.dtr} rts={ser.rts}")
            # 等屏幕复位完成，这段时间内屏幕不响应任何命令。
            if args.settle > 0:
                log(f"等待屏幕复位完成 {args.settle}s ...")
                time.sleep(args.settle)

            # HELLO 带重试：原装图灵屏无应答属正常，只记日志不判失败。
            hello_ok = False
            if not args.no_hello:
                for hi in range(1, args.hello_retries + 1):
                    try:
                        ser.write(bytes([HELLO_CMD]) * 6)
                        ser.flush()
                        resp = ser.read(6)
                        if resp:
                            tag = "USBMONITOR 屏已就绪!" if resp == bytes([1] * 6) else "有应答(非标准)"
                            log(f"HELLO 应答[{hi}]: {resp.hex()} —— {tag}")
                            hello_ok = True
                            break
                        log(f"HELLO 第 {hi}/{args.hello_retries} 次无应答（原装屏属正常）")
                    except Exception as hc:  # noqa: BLE001 - 握手失败不阻断推帧
                        log(f"HELLO 第 {hi} 次异常（忽略）: {hc}")
                    time.sleep(0.4)
                try:
                    ser.reset_input_buffer()
                except Exception:
                    pass
                log("HELLO 结论: " + ("屏幕已响应" if hello_ok else "无应答（原装屏正常，继续推帧）"))

            # ★★ 关键：命令必须分开单独发、每个间隔 0.3s，像素逐块写、末尾 sleep 1s。
            # 早上成功的 push_image_to_screen.py 实测；把方向/亮度/位图头打进一个
            # stream 连发会让屏幕卡在 please run the app（字节全写出但屏幕不认）。
            # build_stream 结构 = 方向16B + 亮度6B + 位图头6B + 像素(余下)
            orientation_pkt = stream[0:16]
            brightness_pkt = stream[16:22]
            bitmap_hdr = stream[22:28]
            pixels = stream[28:]
            for pkt, name in (
                (SCREEN_ON_PKT, "SCREEN_ON"),
                (brightness_pkt, "亮度"),
                (orientation_pkt, "横屏方向"),
                (bitmap_hdr, "位图头"),
            ):
                ser.write(pkt)
                ser.flush()
                log(f"发 {name} ({len(pkt)} B)")
                time.sleep(0.3)

            t0 = time.time()
            chunk_size = 320 * 8  # dev.chunks: width*8 = 2560B/块
            for off in range(0, len(pixels), chunk_size):
                ser.write(pixels[off:off + chunk_size])
            ser.flush()
            log(f"像素写完 {len(pixels)} B，耗时 {time.time() - t0:.2f}s")
            time.sleep(1.0)  # 成功脚本末尾多等 1s 再关串口
            ser.close()
            ser = None
            return True
        except Exception as exc:  # noqa: BLE001 - 串口异常多样，统一重试
            log(f"串口写入失败 {attempt}/{args.retries}: {exc}")
            if ser is not None:
                try:
                    ser.close()
                except Exception:
                    pass
            if attempt < args.retries:
                time.sleep(2)
    return False


def trigger_pending(path: str) -> bool:
    return bool(path) and os.path.exists(path)


def consume_trigger(path: str) -> None:
    if path:
        try:
            os.remove(path)
        except OSError:
            pass


def push_once(data: dict, args, dev: Turing35RevA, port: str | None) -> bool:
    """推一帧的完整动作；返回是否成功送达串口（dry-run 视为成功）。"""
    fp = data_fingerprint(data)
    stale = stale_minutes(data)
    if stale is not None and stale >= args.stale_min:
        log(f"⚠ 数据陈旧：最近活动距今 {stale:.0f} 分钟（阈值 {args.stale_min}）")

    stream = build_stream_for(data, args, dev)

    if args.dry_run:
        log(f"[dry-run] 渲染完成 data={fmt_wan(data['tokens'])} tokens / "
            f"{data['records']} 条 / 字节流 {len(stream)} B，未写串口")
        return True

    if not port:
        log("无可用串口，跳过本轮（屏幕保留上一帧）")
        return False
    ok = send_stream(port, stream, args)
    if ok:
        log(f"已推送 {len(stream)} B → {port} @ {args.baud} "
            f"bitmap_cmd={args.bitmap_cmd} "
            f"rtscts={'off' if args.no_rtscts else 'on'} "
            f"(fp={fp[0]}/{fp[1]})")
    else:
        log("本轮推送失败，下轮重试（屏幕保留上一帧）")
    return ok


# ============================================================================
# 常驻 + 脏矩形局部刷新（--partial）
# ----------------------------------------------------------------------------
# 与上面的短命全帧路径并存，互不干扰。屏幕实测（2026-11-19）：只有串口保持
# 常开时局部帧才被锁存；进程退出关串口后屏幕复位回待机，下一帧局部数据打在
# 空屏上。所以局部刷新必须是常驻会话：RESET 一次 → 全帧建基准 → 后续只推脏矩形。
# ============================================================================

def find_dirty_rect(prev: bytes, cur: bytes, w: int = 320, h: int = 480,
                    block: int = 16) -> tuple[int, int, int, int] | None:
    """按 block×block 比较两帧物理字节，返回变化块包围盒（物理坐标，含端点）。

    块对齐保证矩形边界落在 16 的倍数上，脏区略大于实际变化，换取比较速度快
    （按行切片比较，不逐像素）。无变化返回 None。
    """
    x0, y0, x1, y1 = w, h, -1, -1
    for by in range(0, h, block):
        for bx in range(0, w, block):
            rb = min(block, w - bx) * 2  # 每行字节数（RGB565 = 2B/像素）
            changed = False
            for y in range(by, min(by + block, h)):
                off = (y * w + bx) * 2
                if prev[off:off + rb] != cur[off:off + rb]:
                    changed = True
                    break
            if changed:
                x0, y0 = min(x0, bx), min(y0, by)
                x1, y1 = max(x1, bx + block - 1), max(y1, by + block - 1)
    return (x0, y0, x1, y1) if x1 >= 0 else None


def crop_rect(buf: bytes, w: int, x0: int, y0: int, x1: int, y1: int) -> bytes:
    """从物理帧裁出矩形像素（行序，RGB565），供局部位图头发送。"""
    rw = x1 - x0 + 1
    out = bytearray()
    for y in range(y0, y1 + 1):
        off = (y * w + x0) * 2
        out += buf[off:off + rw * 2]
    return bytes(out)


class ScreenSession:
    """常驻串口会话：RESET 一次点亮，之后复用同一 ser 推全帧/局部帧。"""

    RESET_PKT = bytes([0, 0, 0, 0, 0, 101])
    SCREEN_ON_PKT = bytes([0, 0, 0, 0, 0, 109])

    def __init__(self, port: str, args, dev: Turing35RevA):
        self.port = port
        self.args = args
        self.dev = dev
        self.ser = None
        self.last_frame: bytes | None = None

    def _open(self):
        return serial.Serial(
            self.port, baudrate=self.args.baud,
            timeout=self.args.serial_timeout,
            rtscts=not self.args.no_rtscts,
        )

    def connect(self) -> bool:
        """RESET → 关串口等重枚举 → 重开 → settle。串口保持打开不关。"""
        args = self.args
        ser = None
        try:
            ser = self._open()
            time.sleep(1.0)                      # 打开瞬间屏幕自复位，等它完成
            ser.write(self.RESET_PKT)
            ser.flush()
            log(f"发 RESET 触发屏幕自复位，等 2s")
            time.sleep(2.0)
        except Exception as re:  # noqa: BLE001
            log(f"RESET 阶段异常（忽略，仍走重枚举）: {re}")
        try:
            if ser is not None:
                ser.close()
        except Exception:
            pass
        ser = None
        log(f"关串口等重枚举 {args.reenum_wait}s ...")
        time.sleep(args.reenum_wait)
        for ri in range(1, 11):
            try:
                ser = self._open()
                ser.reset_input_buffer()
                ser.reset_output_buffer()
                log(f"重开串口成功（第 {ri} 次），保持常开")
                break
            except Exception as ropen:
                log(f"重开 {ri}/10 失败: {ropen}")
                time.sleep(1)
        if ser is None:
            log("重枚举后串口无法重新打开")
            return False
        if args.settle > 0:
            time.sleep(args.settle)
        self.ser = ser
        log(f"串口已打开 {self.port}（常驻会话建立）")
        return True

    def push_full(self, frame: bytes) -> None:
        """首次全帧：分开发 SCREEN_ON/亮度/方向/位图头 + 逐块像素。"""
        args, dev, ser = self.args, self.dev, self.ser
        hdr = dev.bitmap_header(0, 0, dev.width - 1, dev.height - 1,
                                cmd=args.bitmap_cmd)
        on = dev.orientation_packet(Orientation.LANDSCAPE
                                    if args.orient == "landscape" else Orientation.PORTRAIT)
        br = dev.brightness_packet(args.brightness)
        for pkt, name in ((self.SCREEN_ON_PKT, "SCREEN_ON"), (br, "亮度"),
                          (on, "方向"), (hdr, "全屏位图头")):
            ser.write(pkt); ser.flush(); time.sleep(0.3)
        t0 = time.time()
        for i in range(0, len(frame), dev.width * 8):
            ser.write(frame[i:i + dev.width * 8])
        ser.flush()
        log(f"全帧 {len(frame)} B 耗时 {time.time() - t0:.2f}s（建基准）")
        self.last_frame = frame

    def push_diff(self, frame: bytes) -> bool:
        """后续帧：与上一帧比脏矩形，只发位图头 + 局部像素。串口不复位。"""
        if self.last_frame is None:
            self.push_full(frame)
            return True
        rect = find_dirty_rect(self.last_frame, frame)
        if rect is None:
            log("两帧无像素变化，跳过")
            return True
        x0, y0, x1, y1 = rect
        crop = crop_rect(frame, self.dev.width, x0, y0, x1, y1)
        hdr = self.dev.bitmap_header(x0, y0, x1, y1, cmd=self.args.bitmap_cmd)
        self.ser.write(hdr); self.ser.flush(); time.sleep(0.05)
        self.ser.write(crop); self.ser.flush()
        ratio = len(crop) / (self.dev.width * self.dev.height * 2) * 100
        log(f"局部刷新 {rect} = {len(hdr) + len(crop)} B "
            f"（全帧的 {ratio:.1f}%，无需 RESET）")
        self.last_frame = frame
        return True

    def close(self) -> None:
        if self.ser is not None:
            try:
                self.ser.close()
            except Exception:
                pass
            self.ser = None


def run_partial_loop(args) -> int:
    """常驻主循环：数据变化 / 时钟每分钟 → 脏矩形局部刷新。"""
    dev = Turing35RevA()
    port = args.port or (None if args.dry_run else auto_port())
    if not args.dry_run and not port:
        log("未发现屏幕串口，可用 --port COMx 指定")
        return 2
    log(f"[局部刷新模式] 启动 port={port or 'dry-run'} interval={args.interval}s "
        f"orient={args.orient}（串口常开，仅首次 RESET）")

    session = None
    if not args.dry_run:
        session = ScreenSession(port, args, dev)
        if not session.connect():
            return 2

    last_fp = None
    last_clock = None
    last_full = time.monotonic()   # 距上次全帧校准的起点；首次 push 会重置
    first = True
    try:
        while True:
            data = load_data(args)
            fp = data_fingerprint(data)
            clock = datetime.now(TZ8).strftime("%H:%M")
            manual = trigger_pending(args.trigger)
            now = time.monotonic()

            # 周期性全帧校准：脏矩形以 last_frame 为基准，一旦屏幕实际显示
            # 与基准失步（局部帧丢包 / 屏幕自复位），数据不变就永远修不回来。
            # 定期强制全帧覆盖整屏并重建基准，让任何失步在一周期内自愈。
            calib_due = (args.full_calibrate_sec > 0 and
                         now - last_full >= args.full_calibrate_sec)

            # 局部模式：时钟每分钟变化也算触发（不像全帧模式刻意排除时钟）
            changed = (fp != last_fp) or (clock != last_clock) or manual
            if first or calib_due or changed:
                do_full = first or calib_due
                if first:
                    reason = "首次"
                elif calib_due:
                    reason = "周期全帧校准"
                elif manual:
                    reason = "手动"
                elif clock != last_clock:
                    reason = "时钟"
                else:
                    reason = "数据变化"
                log(f"触发{'全帧' if do_full else '局部'}刷新（{reason}）clock={clock}")
                frame = build_physical_frame(data, args, dev)
                if args.dry_run:
                    log(f"[dry-run] 物理帧 {len(frame)} B，未写串口")
                elif do_full:
                    session.push_full(frame)
                else:
                    session.push_diff(frame)
                last_fp, last_clock = fp, clock
                if do_full:
                    last_full = time.monotonic()
                consume_trigger(args.trigger)
                first = False
            else:
                left = (args.full_calibrate_sec - (now - last_full)
                        if args.full_calibrate_sec > 0 else -1)
                tail = f"，校准剩 {left:.0f}s" if left >= 0 else ""
                log(f"无变化，跳过（{clock}，下次 {args.interval}s 后{tail}）")

            if args.once:
                break
            time.sleep(args.interval)
    except KeyboardInterrupt:
        log("已停止（Ctrl+C）")
    finally:
        if session is not None:
            session.close()
            log("串口已关闭")
    return 0


def run_loop(args) -> int:
    if args.list:
        if list_ports is None:
            log("缺少 pyserial，无法列出串口")
            return 2
        found = False
        for p in list_ports.comports():
            mark = "  <== 图灵 3.5寸" if (p.vid, p.pid) == TURING_VID_PID else ""
            log(f"{p.device}  VID:PID={p.vid:04x}:{p.pid:04x}{mark}")
            found = found or bool(mark)
        if not found:
            log("未发现精确匹配 1a86:5722 的口（新版屏用通用 CDC 驱动属正常）。"
                "推送时 auto_port 还会按描述/唯一串口兜底，以实际推送日志为准。")
        return 0

    dev = Turing35RevA()
    port = args.port or (None if args.dry_run else auto_port())
    if not args.dry_run and not port:
        log("未自动发现屏幕串口，可用 --port COMx 指定；或 --dry-run 仅自测渲染")
        # 常驻场景下允许先起进程、插屏后自动恢复，故不直接退出
    elif port:
        log(f"目标串口：{port}")

    log(f"启动：interval={args.interval}s orient={args.orient} "
        f"refresh={args.clock_refresh}s trigger={args.trigger or '关闭'}")

    last_fp = None
    last_push = 0.0
    first = True
    try:
        while True:
            now = time.time()
            data = load_data(args)
            fp = data_fingerprint(data)
            manual = trigger_pending(args.trigger)
            time_to_refresh = (now - last_push) >= args.clock_refresh

            # 首次 / 数据变化 / 时钟兜底 / 手动触发 → 推
            need_push = first or (fp != last_fp) or time_to_refresh or manual
            if need_push:
                reason = ("首次" if first else "手动" if manual else
                          "数据变化" if fp != last_fp else "时钟刷新")
                log(f"触发推送（{reason}）")
                ok = push_once(data, args, dev, port)
                if ok:
                    last_fp = fp
                    last_push = now
                    consume_trigger(args.trigger)
                first = False
            else:
                log(f"无变化，跳过（下次 {args.interval}s 后）")

            if args.once:
                return 0
            time.sleep(args.interval)
    except KeyboardInterrupt:
        log("已停止（Ctrl+C）")
        return 0


def parse_args(argv=None):
    ap = argparse.ArgumentParser(description="图灵智显 3.5 寸常驻推送程序")
    ap.add_argument("--port", help="COM 口，如 COM3；不指定则按 VID:PID 自动探测")
    ap.add_argument("--db", default=DEFAULT_DB, help="TokenMonitor 数据库路径")
    ap.add_argument("--interval", type=int, default=60, help="轮询间隔秒（默认 60）")
    ap.add_argument("--clock-refresh", type=int, default=300,
                    help="无数据变化时强制刷新时钟的周期秒（默认 300）")
    ap.add_argument("--orient", choices=["portrait", "landscape"], default="landscape")
    ap.add_argument("--brightness", type=int, default=50,
                    help="亮度百分比 0-100：100 最亮、0 最暗（默认 50）")
    ap.add_argument("--baud", type=int, default=115200)
    ap.add_argument("--bitmap-cmd", type=int, default=197,
                    help="位图命令码：197=开源 DISPLAY_BITMAP（默认）；"
                         "198=官方 UsbMonitor.exe 反编译实测值。屏幕不动时试 198")
    ap.add_argument("--no-rtscts", action="store_true",
                    help="禁用 RTS/CTS 硬件流控（老屏排查用；官方 UsbMonitor 屏默认需开启）")
    ap.add_argument("--no-hello", action="store_true",
                    help="跳过 HELLO 握手（官方 InitializeComm 有此步，读不到应答不影响）")
    ap.add_argument("--serial-timeout", type=int, default=1,
                    help="串口读超时秒（官方驱动用 1；默认 1）")
    ap.add_argument("--no-reset", action="store_true",
                    help="跳过 RESET→关串口→重枚举→重开 序列（实测这是点亮的关键，勿轻易关闭）")
    ap.add_argument("--reenum-wait", type=float, default=4.0,
                    help="发 RESET 关串口后等待 USB 重新枚举的秒数（默认 4）")
    ap.add_argument("--open-delay", type=float, default=0.0,
                    help="打开串口前等待秒数（留出上次关闭后的复位窗口，诊断用）")
    ap.add_argument("--settle", type=float, default=2.0,
                    help="重开串口后等待屏幕稳定的秒数（默认 2）")
    ap.add_argument("--hello-retries", type=int, default=5,
                    help="HELLO 握手重试次数（默认 5，用于确认屏幕是否响应）")
    ap.add_argument("--retries", type=int, default=3, help="串口写失败重试次数（默认 3）")
    ap.add_argument("--stale-min", type=int, default=30, help="数据陈旧告警阈值分钟（默认 30）")
    ap.add_argument("--trigger", default=DEFAULT_TRIGGER,
                    help="手动刷新触发文件路径；传空串禁用")
    ap.add_argument("--demo", action="store_true", help="用样例数据，不读库")
    ap.add_argument("--dry-run", action="store_true", help="只渲染，不写串口（自测用）")
    ap.add_argument("--once", action="store_true", help="只推一帧就退出")
    ap.add_argument("--partial", action="store_true",
                    help="常驻局部刷新模式：串口保持常开，首次全帧后只推脏矩形；"
                         "时钟每分钟即时刷新，无需 RESET（黑屏问题治本）")
    ap.add_argument("--full-calibrate-sec", type=int, default=600,
                    help="局部模式下每隔 N 秒强制全帧校准一次，重建 last_frame 基准，"
                         "自愈屏幕与基准的失步（默认 600=10 分钟；0=禁用）")
    ap.add_argument("--list", action="store_true", help="列出串口后退出")
    return ap.parse_args(argv)


def main() -> int:
    setup_console_utf8()
    args = parse_args()
    if args.partial and not args.list:
        return run_partial_loop(args)
    return run_loop(args)


if __name__ == "__main__":
    sys.exit(main())
