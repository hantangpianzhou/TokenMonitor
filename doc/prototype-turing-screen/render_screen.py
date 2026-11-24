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

# 结构色：底、卡片、描边、分隔线、文字。**恒定，不随主题变** —— 软件里的主题
# 本来也只换 accent、底色不动，屏上同构处理才与软件体感一致；且 RGB565 只有
# 5/6/5 位色深，深底上的 1px 描边一旦往彩色偏就会糊成一片。
BG = "#0A1120"
PANEL = "#131D31"
BORDER = "#26365A"
GRID = "#1C2942"
TEXT = "#EDF2FA"
MUTED = "#8FA3C4"

# 主题 key → 屏端两个「数据色」：(主色, 辅色)
#   主色 = 今日 TOKEN 大数字、占比百分比、比例条填充
#   辅色 = logo 色块、live 点、「今日活跃 N 次」
# 刻意不复用 Rust 侧 `ThemeColor::accent()`：UI 的 Ocean 是 #3b82f6，在 BG 上
# 对比度只有 4.2:1，比屏端现行 #5B9CF8（6.8:1）暗一档 —— 3.5" 屏 + 强环境光
# 下读不清。屏端各主题统一取比 UI accent 亮一级的色阶。
# ocean 一行就是本功能上线前的原配色，默认主题零视觉回归。
SCREEN_PALETTES: dict[str, tuple[str, str]] = {
    "ocean": ("#5B9CF8", "#3FD0C9"),
    "jade": ("#34D399", "#22D3EE"),
    "amber": ("#FBBF24", "#FB923C"),
    "rose": ("#FB7185", "#F0ABFC"),
}
DEFAULT_THEME = "ocean"


def palette_for(theme: str | None) -> tuple[str, str]:
    """主题 key → (主色, 辅色)；缺失或未知一律回退 ocean（即原配色）。"""
    return SCREEN_PALETTES.get(theme or "", SCREEN_PALETTES[DEFAULT_THEME])


PROVIDER_CN = {
    "workbuddy": "WorkBuddy", "claude": "Claude", "codex": "Codex",
    "gemini": "Gemini", "deepseek": "DeepSeek", "trae": "Trae",
    "qoder": "Qoder", "atomcode": "AtomCode", "codearts": "CodeArts",
    "teleagent": "TeleAgent", "codebuddy": "CodeBuddy", "opencode": "OpenCode",
    "openclaw": "OpenClaw", "antigravity": "Antigravity", "pi": "Pi",
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


def load_theme_key(conn) -> str:
    """读 settings 表的 theme.color（与 Rust 侧 `THEME_COLOR_KEY` 同名）。

    主题只在主程序「设置 → 外观」里改，落库即唯一真值源，屏端不需要主程序
    额外传参 —— 复用 load_from_db 已打开的那条只读连接，成本为零。
    旧库没有 settings 表、或行被清掉、或值是脏数据，都回退默认主题，
    绝不因为配色读不到就让整帧渲染失败。
    """
    try:
        row = conn.execute(
            "SELECT value FROM settings WHERE key = 'theme.color'").fetchone()
    except sqlite3.Error:
        return DEFAULT_THEME
    return row[0] if row and row[0] in SCREEN_PALETTES else DEFAULT_THEME


def load_from_db(db_path: str) -> dict:
    conn = sqlite3.connect("file:" + db_path + "?mode=ro", uri=True)
    theme = load_theme_key(conn)
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
        "theme": theme,
        "env": collect_env(),
    }


def demo_data() -> dict:
    return {
        "tokens": 1_437_931, "records": 17, "last_activity": "22:41",
        "providers": [("WorkBuddy", 1_437_931)],
        "updated": "22:42", "date": "2026-11-15",
        "theme": DEFAULT_THEME,
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


def bar(d: ImageDraw.ImageDraw, box, frac: float, color: str) -> None:
    """带深色轨道的比例条；极小非零值也留最小可见宽度。

    color 必填而不给默认值：默认参数在 def 时求值，会悄悄钉死某一个主题的
    主色，让 `palette_for` 的切换在比例条上失效。
    """
    x0, y0, x1, y1 = box
    d.rounded_rectangle(box, radius=3, fill=GRID)
    if frac > 0:
        fx = min(x1, x0 + max(5, int((x1 - x0) * frac)))
        d.rounded_rectangle((x0, y0, fx, y1), radius=3, fill=color)


def big_number(d: ImageDraw.ImageDraw, x: int, y: int, v: int,
               size: int, unit_size: int, color: str) -> None:
    """数字大、单位小，底边严格对齐（按 bbox 实测，不靠字号差猜偏移）。

    所有造型参数均必填：默认参数在 def 时求值，会把某个主题的颜色和字号
    永久钉在这一份代码上。
    """
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
    # 配色随 data["theme"]（load_from_db 从 settings 表读入）走。保持 render
    # 单一入口：screen_loop 的每轮推送和本文件的 main() 都不必多传参数，
    # 主题变化自然体现在下一帧上。
    accent, accent2 = palette_for(data.get("theme"))
    w, h = (H, W) if portrait else (W, H)
    img = Image.new("RGB", (w, h), BG)
    d = ImageDraw.Draw(img)
    M = 16

    # ---- 标题栏：小色块 logo + live 点 + 日期（统一 16px 网格）----
    d.rectangle((0, 0, w, 42), fill=PANEL)
    d.rectangle((0, 41, w, 42), fill=BORDER)
    d.rounded_rectangle((M - 2, 13, M + 6, 29), radius=2, fill=accent2)
    d.text((M + 14, 11), "TOKEN MONITOR", font=font(15, True), fill=TEXT)
    d.ellipse((w - 98, 19, w - 92, 25), fill=accent2)
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
        big_number(d, lx + 14, ct + 30, data["tokens"],
                   size=34, unit_size=17, color=accent)

        d.text((rx + 14, ct + 9), "今日活跃", font=font(12), fill=MUTED)
        d.text((rx + cw - 14, ct + 10), f"最近 {data.get('last_activity', '--:--')}",
               font=font(11), fill=MUTED, anchor="ra")
        rec = str(data.get("records", 0))
        fr = font(34, True)
        d.text((rx + 14, ct + 28), rec, font=fr, fill=accent2, anchor="la")
        rnw = d.textlength(rec, font=fr)
        rb = d.textbbox((0, 0), rec, font=fr, anchor="la")
        fu = font(15, True)
        ub = d.textbbox((0, 0), "次", font=fu, anchor="la")
        d.text((rx + 14 + rnw + 8, ct + 28 + (ub[1] - rb[1]) + (rb[3] - ub[3])),
               "次", font=fu, fill=accent2, anchor="la")

        # ---- 底部：左 = Agent 分布（2/3），右 = 时间（1/3）----
        # 文档第6章：原版 3:1 改成 2:1，时间区更宽松，可上更大字号。
        by = 150
        lw = (w - M * 2) * 2 // 3          # 左列宽 ≈298（2/3）
        div_x = M + lw                      # 分隔线位置
        rx = div_x + 12                     # 右列起点
        rcw = w - M - rx                    # 右列宽 ≈148（1/3）

        # 左列 Agent 分布：top 3，名称 + 占比 + 比例条
        d.text((M, by), "AGENT 分布", font=font(12), fill=MUTED)
        d.text((div_x, by), "占比", font=font(11), fill=MUTED, anchor="ra")
        agents = data["providers"][:3] or [("-", 0)]
        for i, (name, v) in enumerate(agents):
            ry = by + 22 + i * 40
            d.text((M, ry), name, font=font(14), fill=TEXT)
            d.text((div_x, ry + 1), f"{v / total_p * 100:.0f}%",
                   font=font(13), fill=accent, anchor="ra")
            bar(d, (M, ry + 20, div_x, ry + 26), v / total_p, accent)

        # 分隔线
        d.rectangle((div_x + 4, by, div_x + 5, h - 14), fill=GRID)

        # 右列 时间：标题在上，大时钟垂直居中于剩余空间
        cx = rx + rcw // 2
        env = data.get("env", default_env())
        d.text((cx, by), "当前时间", font=font(11), fill=MUTED, anchor="ma")
        clock = env.get("clock", "--:--")
        fc = font(56, True)
        while fc.size > 24 and d.textlength(clock, font=fc) > rcw - 12:
            fc = font(fc.size - 4, True)   # 超宽自适应，避免被右侧裁切
        # 垂直居中：anchor="ma" 的锚点是 ascender 线，墨迹实际占 [cy+cb[1], cy+cb[3]]
        # 让墨迹中心落在「标题下方 ~ 底部留白」区域的中心，而不是把 cy 当顶部算。
        cb = d.textbbox((0, 0), clock, font=fc, anchor="ma")
        zone_top, zone_bottom = by + 30, h - 14
        cy = (zone_top + zone_bottom) / 2 - (cb[1] + cb[3]) / 2
        d.text((cx, cy), clock, font=fc, fill=TEXT, anchor="ma")
    else:
        # ---- 竖屏：卡片上下排列 ----
        card(d, (M, 52, w - M, 140))
        d.text((M + 14, 62), "今日 TOKEN 用量", font=font(12), fill=MUTED)
        big_number(d, M + 14, 84, data["tokens"],
                   size=40, unit_size=19, color=accent)

        card(d, (M, 150, w - M, 238))
        d.text((M + 14, 160), "今日活跃", font=font(12), fill=MUTED)
        d.text((w - M - 14, 161), f"最近 {data.get('last_activity', '--:--')}",
               font=font(11), fill=MUTED, anchor="ra")
        rec = str(data.get("records", 0))
        fr = font(34, True)
        d.text((M + 14, 180), rec, font=fr, fill=accent2, anchor="la")
        rnw = d.textlength(rec, font=fr)
        rb = d.textbbox((0, 0), rec, font=fr, anchor="la")
        fu = font(15, True)
        ub = d.textbbox((0, 0), "次", font=fu, anchor="la")
        d.text((M + 14 + rnw + 8, 180 + (ub[1] - rb[1]) + (rb[3] - ub[3])),
               "次", font=fu, fill=accent2, anchor="la")

        ay = 248
        d.text((M, ay), "AGENT 分布", font=font(12), fill=MUTED)
        d.text((w - M, ay), "占比", font=font(11), fill=MUTED, anchor="ra")
        d.text((M, ay + 20), top[0], font=font(13), fill=TEXT)
        d.text((w - M, ay + 21), f"{top[1] / total_p * 100:.0f}%",
               font=font(12), fill=accent, anchor="ra")
        bar(d, (M, ay + 40, w - M, ay + 46), top[1] / total_p, accent)

        # ---- 大时钟：仅时间 ----
        ey = 300
        d.rectangle((M, ey - 6, w - M, ey - 5), fill=GRID)
        d.text((M, ey), "当前时间", font=font(12), fill=MUTED)
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
