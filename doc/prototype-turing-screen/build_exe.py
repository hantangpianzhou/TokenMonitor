#!/usr/bin/env python3
"""方案1：用 PyInstaller 把常驻推送程序打包成单文件 exe。

产物：dist/TokenScreen.exe —— 目标机零 Python 依赖，双击即运行（读同机 TokenMonitor 的库）。

用法：
    python build_exe.py                 # 打包
    python build_exe.py --clean         # 清理旧构建后再打包
    python build_exe.py --check         # 只检查打包环境，不打包

前置：pip install pyinstaller pyserial pillow
"""
from __future__ import annotations

import argparse
import os
import shutil
import subprocess
import sys
from pathlib import Path

HERE = Path(__file__).resolve().parent
ENTRY = HERE / "screen_loop.py"
NAME = "TokenScreen"
DIST = HERE / "dist"
BUILD = HERE / "build"
SPEC = HERE / f"{NAME}.spec"


def check_env() -> bool:
    ok = True
    for mod, hint in (("PyInstaller", "pip install pyinstaller"),
                      ("serial", "pip install pyserial"),
                      ("PIL", "pip install pillow")):
        try:
            m = __import__(mod)
            ver = getattr(m, "__version__", "?")
            print(f"  [OK] {mod} {ver}")
        except ImportError:
            print(f"  [缺失] {mod}  → {hint}")
            ok = False
    if not ENTRY.exists():
        print(f"  [缺失] 入口程序 {ENTRY}")
        ok = False
    return ok


def clean() -> None:
    for p in (DIST, BUILD, SPEC):
        if p.is_dir():
            shutil.rmtree(p)
            print(f"  删除 {p.name}/")
        elif p.exists():
            p.unlink()
            print(f"  删除 {p.name}")


def build() -> int:
    # 关键参数：
    #  --onefile     单文件 exe，拷贝方便
    #  --noconsole   去掉黑框？=> 保留控制台！日志对用户可见，便于排查串口/数据问题
    #  --hidden-import  pyserial 是延迟 import（screen_loop 里 try），需显式带上
    #  --add-data    无（字体已改为系统多路径探测，不打字体，避免版权与体积）
    cmd = [
        sys.executable, "-m", "PyInstaller",
        "--onefile",
        "--name", NAME,
        "--hidden-import", "serial",
        "--hidden-import", "serial.tools.list_ports",
        "--hidden-import", "render_screen",
        "--hidden-import", "send_to_screen",
        "--clean",
        "--distpath", str(DIST),
        "--workpath", str(BUILD),
        "--specpath", str(HERE),
        str(ENTRY),
    ]
    print("执行：", " ".join(cmd))
    rc = subprocess.run(cmd, cwd=str(HERE)).returncode
    if rc == 0:
        exe = DIST / (NAME + ".exe")
        size_mb = exe.stat().st_size / 1024 / 1024 if exe.exists() else 0
        print(f"\n打包成功：{exe}  ({size_mb:.1f} MB)")
        print("验证：dist\\TokenScreen.exe --demo --dry-run --once")
        print("部署：把 exe 拷到目标机，与 TokenMonitor.exe 同机运行即可（数据来自同机 SQLite）")
    else:
        print(f"\n打包失败，返回码 {rc}")
    return rc


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--clean", action="store_true", help="打包前清理旧构建")
    ap.add_argument("--check", action="store_true", help="只检查环境")
    args = ap.parse_args()

    print("== 打包环境检查 ==")
    if not check_env():
        print("环境不满足，请先安装缺失依赖")
        return 2
    if args.check:
        return 0
    if args.clean:
        print("\n== 清理旧构建 ==")
        clean()
    print("\n== 开始打包 ==")
    return build()


if __name__ == "__main__":
    sys.exit(main())
