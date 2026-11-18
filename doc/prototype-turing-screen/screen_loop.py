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
    """按 VID:PID 自动探测图灵 3.5 寸串口；找不到返回 None。"""
    if list_ports is None:
        return None
    for p in list_ports.comports():
        if p.vid == TURING_VID_PID[0] and p.pid == TURING_VID_PID[1]:
            return p.device
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


def build_stream_for(data: dict, args, dev: Turing35RevA) -> bytes:
    """渲染 → RGB565 → 按方向重排 → 完整协议字节流。纯内存，不落盘。"""
    img = render(data, portrait=(args.orient == "portrait"))
    frame = to_rgb565_le(img)
    orientation = Orientation.LANDSCAPE if args.orient == "landscape" else Orientation.PORTRAIT
    if orientation == Orientation.LANDSCAPE:
        frame = dev.rotate_cw(frame)
    return dev.build_stream(frame, orientation, args.brightness)


def send_stream(port: str, stream: bytes, args) -> bool:
    """断链重试：全失败返回 False（保留上一帧），由调用方决定下轮再试。"""
    if serial is None:
        log("缺少 pyserial：pip install pyserial（打包时已内置则不会遇到）")
        return False
    for attempt in range(1, args.retries + 1):
        try:
            with serial.Serial(port, baudrate=args.baud, timeout=5) as ser:
                ser.write(stream)
                ser.flush()
            return True
        except Exception as exc:  # noqa: BLE001 - 串口异常多样，统一重试
            log(f"串口写入失败 {attempt}/{args.retries}: {exc}")
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
            f"(fp={fp[0]}/{fp[1]})")
    else:
        log("本轮推送失败，下轮重试（屏幕保留上一帧）")
    return ok


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
            log("未发现图灵 3.5 寸（VID:PID 1a86:5722），检查 USB 线/扩展坞数据口")
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
    ap.add_argument("--brightness", type=int, default=25, help="0 最亮，255 最暗（默认 25）")
    ap.add_argument("--baud", type=int, default=115200)
    ap.add_argument("--retries", type=int, default=3, help="串口写失败重试次数（默认 3）")
    ap.add_argument("--stale-min", type=int, default=30, help="数据陈旧告警阈值分钟（默认 30）")
    ap.add_argument("--trigger", default=DEFAULT_TRIGGER,
                    help="手动刷新触发文件路径；传空串禁用")
    ap.add_argument("--demo", action="store_true", help="用样例数据，不读库")
    ap.add_argument("--dry-run", action="store_true", help="只渲染，不写串口（自测用）")
    ap.add_argument("--once", action="store_true", help="只推一帧就退出")
    ap.add_argument("--list", action="store_true", help="列出串口后退出")
    return ap.parse_args(argv)


def main() -> int:
    setup_console_utf8()
    return run_loop(parse_args())


if __name__ == "__main__":
    sys.exit(main())
