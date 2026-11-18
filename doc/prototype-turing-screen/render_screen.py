#!/usr/bin/env python3
"""图灵智显 3.5 寸小屏 UI 原型 — 使用 TokenMonitor 真实数据渲染预览 + RGB565。

不依赖硬件。屏到手后，把 --emit-rgb565 导出的 .bin 按协议整帧发过去即可。

用法：
    python render_screen.py                       # 读真实 DB，生成横屏预览
    python render_screen.py --orient portrait     # 竖屏布局
    python render_screen.py --demo                # 无 DB 时用样例数据
    python render_screen.py --json data.json      # 从外部 JSON 读聚合结果
"""
from __future__ import annotations

import argparse
import json
import os
import sqlite3
from datetime import datetime, timedelta, timezone

from PIL import Image, ImageDraw, ImageFont

TZ8 = timezone(timedelta(hours=8))
W, H = 480, 320


def _pick_font(*names: str) -> str:
    """首个存在的字体路径；都不存在则回退 PIL 默认（英文 Windows 不崩）。"""
    for name in names:
        for base in (os.path.join(os.environ.get("WINDIR", "C:/Windows"), "Fonts"),
                     "/usr/share/fonts", "/System/Library/Fonts"):
            p = os.path.join(base, name)
            if os.path.exists(p):
                return p
    return ""  # font() 里回退 load_default


FONT_REG = _pick_font("msyh.ttc", "simhei.ttf", "simsun.ttc", "Deng.ttf", "NotoSansCJK-Regular.ttc")
FONT_BOLD = _pick_font("msyhbd.ttc", "msyh.ttc", "simhei.ttf", "NotoSansCJK-Bold.ttc")

# 收敛的深色仪表盘配色：一个主色 + 一个辅色，去掉跳脱的纯绿与金额色。
BG = "#0A1120"
PANEL = "#131D31"
BORDER = "#26365A"
GRID = "#1C2942"
TEXT = "#EDF2FA"
MUTED = "#8FA3C4"
ACCENT = "#5B9CF8"
CYAN = "#3FD0C9"
BAR_DIM = "#335A94"

PROVIDER_CN = {
    "workbuddy": "WorkBuddy", "claude": "Claude", "codex": "Codex",
    "gemini": "Gemini", "deepseek": "DeepSeek", "trae": "Trae",
    "qoder": "Qoder", "atomcode": "AtomCode", "codearts": "CodeArts",
}


def fmt_wan(v: int) -> str:
    """复刻 format_tokens_wan：亿/千万/百万/万，万为下限单位。"""
    units = [(1e8, "亿"), (1e7, "千万"), (1e6, "百万"), (1e4, "万")]
    t = float(v)
    idx = next((i for i, (s, _) in enumerate(units) if t >= s), len(units) - 1)
    while idx > 0:
        m = t / units[idx][0]
        if m < 10.0 and round(m, 1) >= 10.0:
            idx -= 1
        else:
            break
    scale, suffix = units[idx]
    m = t / scale
    dec = 2 if m < 1 else 1
    s = f"{m:.{dec}f}".rstrip("0").rstrip(".")
    return f"{s}{suffix}"


def split_wan(v: int) -> tuple[str, str]:
    """把「1.4百万」拆成数字与单位，供主数字大小分级。"""
    s = fmt_wan(v)
    for u in ("亿", "千万", "百万", "万"):
        if s.endswith(u):
            return s[:-len(u)], u
    return s, ""


def load_from_db(db_path: str) -> dict:
    conn = sqlite3.connect("file:" + db_path + "?mode=ro", uri=True)
    now = datetime.now(TZ8)
    day_start = now.replace(hour=0, minute=0, second=0, microsecond=0)
    start = (day_start.astimezone(timezone.utc)).isoformat()
    end = (now.astimezone(timezone.utc)).isoformat()

    def total(where: str, params: tuple) -> int:
        row = conn.execute(
            f"SELECT COALESCE(SUM(input_tokens+output_tokens+cache_read_tokens+cache_write_tokens),0) "
            f"FROM usage_records WHERE {where}", params).fetchone()
        return int(row[0])

    today_tokens = total("started_at>=? AND started_at<?", (start, end))
    recs = conn.execute(
        "SELECT COUNT(*) FROM usage_records "
        "WHERE started_at>=? AND started_at<?", (start, end)).fetchone()[0]
    last_ts = conn.execute(
        "SELECT MAX(started_at) FROM usage_records "
        "WHERE started_at>=? AND started_at<?", (start, end)).fetchone()[0]
    last_activity = "--:--"
    if last_ts:
        try:
            lt = datetime.fromisoformat(str(last_ts).replace("Z", "+00:00"))
            if lt.tzinfo is None:
                lt = lt.replace(tzinfo=timezone.utc)
            last_activity = lt.astimezone(TZ8).strftime("%H:%M")
        except (ValueError, TypeError):
            last_activity = "--:--"

    providers = [
        (PROVIDER_CN.get(r[0], r[0]), int(r[1]))
        for r in conn.execute(
            "SELECT provider,SUM(input_tokens+output_tokens+cache_read_tokens+cache_write_tokens) t "
            "FROM usage_records WHERE started_at>=? AND started_at<? "
            "GROUP BY provider ORDER BY t DESC", (start, end)).fetchall()
    ]

    week = []
    for i in range(6, -1, -1):
        d = (day_start - timedelta(days=i))
        s = d.astimezone(timezone.utc).isoformat()
        e = (d + timedelta(days=1)).astimezone(timezone.utc).isoformat()
        week.append((d.strftime("%m/%d"), total("started_at>=? AND started_at<?", (s, e))))
    conn.close()
    return {
        "tokens": today_tokens, "records": int(recs),
        "last_activity": last_activity,
        "providers": providers, "updated": now.strftime("%H:%M"),
        "date": now.strftime("%Y-%m-%d"),
        "env": collect_env(),
    }


def demo_data() -> dict:
    return {
        "tokens": 1_437_931, "records": 17, "last_activity": "22:41",
        "providers": [("WorkBuddy", 1_437_931)],
        "updated": "22:42", "date": "2026-11-15",
        "env": {"clock": "22:42"},
    }


def collect_env() -> dict:
    """真实时钟（东八区）；天气已移除，env 只保留 clock。"""
    return {"clock": datetime.now(TZ8).strftime("%H:%M")}


def font(size: int, bold: bool = False):
    path = FONT_BOLD if bold else FONT_REG
    if path:
        return ImageFont.truetype(path, size)
    return ImageFont.load_default(size)  # 无中文字体时至少不崩（显示为方框）


def card(d: ImageDraw.ImageDraw, box) -> None:
    d.rounded_rectangle(box, radius=10, fill=PANEL, outline=BORDER, width=1)


def bar(d: ImageDraw.ImageDraw, box, frac: float, color: str = ACCENT) -> None:
    """带深色轨道的比例条；极小非零值也留最小可见宽度。"""
    x0, y0, x1, y1 = box
    d.rounded_rectangle(box, radius=3, fill=GRID)
    if frac > 0:
        fx = min(x1, x0 + max(5, int((x1 - x0) * frac)))
        d.rounded_rectangle((x0, y0, fx, y1), radius=3, fill=color)


def big_number(d: ImageDraw.ImageDraw, x: int, y: int, v: int,
               size: int = 34, unit_size: int = 17, color: str = ACCENT) -> None:
    """数字大、单位小，底边严格对齐（按 bbox 实测，不靠字号差猜偏移）。"""
    num, unit = split_wan(v)
    fn = font(size, True)
    d.text((x, y), num, font=fn, fill=color, anchor="la")
    if unit:
        fu = font(unit_size, True)
        nb = d.textbbox((0, 0), num, font=fn, anchor="la")
        ub = d.textbbox((0, 0), unit, font=fu, anchor="la")
        nw = nb[2] - nb[0]
        d.text((x + nw + 6, y + (ub[1] - nb[1]) + (nb[3] - ub[3])), unit,
               font=fu, fill=color, anchor="la")


def default_env() -> dict:
    """外部 JSON 缺 env 时的兜底，保证渲染不崩且不伪装实时数据。"""
    return {"clock": "--:--"}


def draw_clock(d: ImageDraw.ImageDraw, env: dict, *, x: int, y: int,
               clock_size: int, center_w: int | None = None) -> None:
    """大时钟：仅时间，无天气/DEMO。传 center_w 时水平居中。"""
    clock = env.get("clock", "--:--")
    fc = font(clock_size, True)
    cb = d.textbbox((0, 0), clock, font=fc, anchor="la")
    clock_w = cb[2] - cb[0]
    cx = x + (center_w - clock_w) // 2 if center_w is not None else x
    d.text((cx, y), clock, font=fc, fill=TEXT, anchor="la")


def render(data: dict, portrait: bool = False) -> Image.Image:
    w, h = (H, W) if portrait else (W, H)
    img = Image.new("RGB", (w, h), BG)
    d = ImageDraw.Draw(img)
    M = 16

    # ---- 标题栏：小色块 logo + live 点 + 日期（统一 16px 网格）----
    d.rectangle((0, 0, w, 42), fill=PANEL)
    d.rectangle((0, 41, w, 42), fill=BORDER)
    d.rounded_rectangle((M - 2, 13, M + 6, 29), radius=2, fill=CYAN)
    d.text((M + 14, 11), "TOKEN MONITOR", font=font(15, True), fill=TEXT)
    d.ellipse((w - 98, 19, w - 92, 25), fill=CYAN)
    d.text((w - M, 13), data["date"], font=font(12), fill=MUTED, anchor="ra")

    total_p = sum(p[1] for p in data["providers"]) or 1
    top = data["providers"][0] if data["providers"] else ("-", 0)

    if not portrait:
        # ---- 用量 / 费用：两张等高卡片，左右严格对齐 ----
        ct, cb, gap = 52, 134, 10
        cw = (w - M * 2 - gap) // 2
        lx, rx = M, M + cw + gap
        card(d, (lx, ct, lx + cw, cb))
        card(d, (rx, ct, rx + cw, cb))

        d.text((lx + 14, ct + 9), "今日 TOKEN", font=font(12), fill=MUTED)
        big_number(d, lx + 14, ct + 30, data["tokens"], size=34, unit_size=17)

        d.text((rx + 14, ct + 9), "今日活跃", font=font(12), fill=MUTED)
        d.text((rx + cw - 14, ct + 10), f"最近 {data.get('last_activity', '--:--')}",
               font=font(11), fill=MUTED, anchor="ra")
        rec = str(data.get("records", 0))
        fr = font(34, True)
        d.text((rx + 14, ct + 28), rec, font=fr, fill=CYAN, anchor="la")
        rnw = d.textlength(rec, font=fr)
        rb = d.textbbox((0, 0), rec, font=fr, anchor="la")
        fu = font(15, True)
        ub = d.textbbox((0, 0), "次", font=fu, anchor="la")
        d.text((rx + 14 + rnw + 8, ct + 28 + (ub[1] - rb[1]) + (rb[3] - ub[3])),
               "次", font=fu, fill=CYAN, anchor="la")

        # ---- 底部：左 = Agent 分布（3/4），右 = 时间天气（1/4）----
        by = 150
        lw = (w - M * 2) * 3 // 4          # 左列宽 336
        div_x = M + lw                      # 分隔线位置
        rx = div_x + 12                     # 右列起点
        rcw = w - M - rx                    # 右列宽

        # 左列 Agent 分布：top 3，名称 + 占比 + 比例条
        d.text((M, by), "AGENT 分布", font=font(12), fill=MUTED)
        d.text((div_x, by), "占比", font=font(11), fill=MUTED, anchor="ra")
        agents = data["providers"][:3] or [("-", 0)]
        for i, (name, v) in enumerate(agents):
            ry = by + 22 + i * 40
            d.text((M, ry), name, font=font(13), fill=TEXT)
            d.text((div_x, ry + 1), f"{v / total_p * 100:.0f}%",
                   font=font(12), fill=ACCENT, anchor="ra")
            bar(d, (M, ry + 20, div_x, ry + 25), v / total_p)

        # 分隔线
        d.rectangle((div_x + 4, by, div_x + 5, h - 14), fill=GRID)

        # 右列 时间：标题在上，大时钟垂直居中于剩余空间
        cx = rx + rcw // 2
        env = data.get("env", default_env())
        d.text((cx, by), "当前时间", font=font(11), fill=MUTED, anchor="ma")
        clock = env.get("clock", "--:--")
        fc = font(40, True)
        cb = d.textbbox((0, 0), clock, font=fc, anchor="ma")
        clock_h = cb[3] - cb[1]
        cy = by + 30 + ((h - 20) - (by + 30) - clock_h) // 2
        d.text((cx, cy), clock, font=fc, fill=TEXT, anchor="ma")
        d.text((cx, h - 16), "更新 " + data["updated"],
               font=font(10), fill=MUTED, anchor="ma")
    else:
        # ---- 竖屏：卡片上下排列 ----
        card(d, (M, 52, w - M, 140))
        d.text((M + 14, 62), "今日 TOKEN 用量", font=font(12), fill=MUTED)
        big_number(d, M + 14, 84, data["tokens"], size=40, unit_size=19)

        card(d, (M, 150, w - M, 238))
        d.text((M + 14, 160), "今日活跃", font=font(12), fill=MUTED)
        d.text((w - M - 14, 161), f"最近 {data.get('last_activity', '--:--')}",
               font=font(11), fill=MUTED, anchor="ra")
        rec = str(data.get("records", 0))
        fr = font(34, True)
        d.text((M + 14, 180), rec, font=fr, fill=CYAN, anchor="la")
        rnw = d.textlength(rec, font=fr)
        rb = d.textbbox((0, 0), rec, font=fr, anchor="la")
        fu = font(15, True)
        ub = d.textbbox((0, 0), "次", font=fu, anchor="la")
        d.text((M + 14 + rnw + 8, 180 + (ub[1] - rb[1]) + (rb[3] - ub[3])),
               "次", font=fu, fill=CYAN, anchor="la")

        ay = 248
        d.text((M, ay), "AGENT 分布", font=font(12), fill=MUTED)
        d.text((w - M, ay), "占比", font=font(11), fill=MUTED, anchor="ra")
        d.text((M, ay + 20), top[0], font=font(13), fill=TEXT)
        d.text((w - M, ay + 21), f"{top[1] / total_p * 100:.0f}%",
               font=font(12), fill=ACCENT, anchor="ra")
        bar(d, (M, ay + 40, w - M, ay + 46), top[1] / total_p)

        # ---- 大时钟：仅时间 ----
        ey = 300
        d.rectangle((M, ey - 6, w - M, ey - 5), fill=GRID)
        d.text((M, ey), "当前时间", font=font(12), fill=MUTED)
        d.text((w - M, ey + 1), "更新 " + data["updated"],
               font=font(10), fill=MUTED, anchor="ra")
        draw_clock(d, data.get("env", default_env()),
                   x=M, y=ey + 22, clock_size=72, center_w=w - M * 2)
    return img


def to_rgb565_le(img: Image.Image) -> bytes:
    """小端 RGB565，与社区 gist 的 rgb888_to_rgb565 输出顺序一致。"""
    out = bytearray()
    px = img.convert("RGB").tobytes()
    for i in range(0, len(px), 3):
        r, g, b = px[i], px[i + 1], px[i + 2]
        word = ((r & 0xF8) << 8) | ((g & 0xFC) << 3) | (b >> 3)
        out.append(word & 0xFF)
        out.append(word >> 8)
    return bytes(out)


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("--orient", choices=["landscape", "portrait"], default="landscape")
    ap.add_argument("--db", default=os.path.expanduser(
        "~/AppData/Roaming/TokenMonitor/tokenmonitor.db"))
    ap.add_argument("--json", help="从 JSON 文件读聚合结果")
    ap.add_argument("--demo", action="store_true")
    ap.add_argument("--emit-rgb565", action="store_true", help="同时导出 .bin")
    ap.add_argument("--out", default=os.path.dirname(os.path.abspath(__file__)))
    args = ap.parse_args()

    if args.demo:
        data = demo_data()
    elif args.json:
        with open(args.json, encoding="utf-8") as f:
            data = json.load(f)
    else:
        data = load_from_db(args.db)

    img = render(data, portrait=args.orient == "portrait")
    tag = args.orient
    png = os.path.join(args.out, f"preview_{tag}.png")
    img.save(png)
    print(f"PNG: {png}")

    if args.emit_rgb565:
        raw = to_rgb565_le(img)
        bin_path = os.path.join(args.out, f"frame_{tag}.rgb565")
        with open(bin_path, "wb") as f:
            f.write(raw)
        print(f"RGB565: {bin_path} ({len(raw)} bytes)")

    print(f"data: {fmt_wan(data['tokens'])} tokens / {data['records']} 条 / "
          f"最近 {data.get('last_activity', '--:--')}")


if __name__ == "__main__":
    main()
