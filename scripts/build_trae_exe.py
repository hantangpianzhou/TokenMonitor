#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""把 trae_collector.py 打包成单文件 exe，供 TokenMonitor 主程序内嵌自动采集。

产物：scripts/dist/TraeCollector.exe
  —— 目标机零 Python 依赖；主程序每轮扫描拉起 `--auto`：缓存 key 有效则复用，
     失效则自动从内存提 key 并存缓存（失败写 30 分钟冷却）。含 pycryptodome
     （SQLCipher 解密所需）。

用法：
    python build_trae_exe.py             # 打包
    python build_trae_exe.py --clean     # 清理旧构建后再打包
    python build_trae_exe.py --check     # 只检查打包环境

打包后需把产物复制到 assets/TokenCollector.exe 再 cargo build --release。
"""
from __future__ import annotations

import argparse
import shutil
import subprocess
import sys
from pathlib import Path

HERE = Path(__file__).resolve().parent
ENTRY = HERE / "trae_collector.py"
NAME = "TraeCollector"
DIST = HERE / "dist"
BUILD = HERE / "build"
SPEC = HERE / f"{NAME}.spec"


def check_env() -> bool:
    ok = True
    for mod, hint in (("PyInstaller", "pip install pyinstaller"),
                      ("Crypto", "pip install pycryptodome")):
        try:
            m = __import__(mod)
            print(f"  [OK] {mod} {getattr(m, '__version__', '?')}")
        except ImportError:
            print(f"  [缺失] {mod}  -> {hint}")
            ok = False
    if not ENTRY.exists():
        print(f"  [缺失] 入口程序 {ENTRY}")
        ok = False
    return ok


def clean() -> None:
    for p in (DIST / NAME, BUILD / NAME, SPEC):
        if p.is_dir():
            shutil.rmtree(p)
            print(f"  删除 {p}/")
        elif p.exists():
            p.unlink()
            print(f"  删除 {p}")


def build() -> int:
    cmd = [
        sys.executable, "-m", "PyInstaller",
        "--onefile",
        "--name", NAME,
        "--hidden-import", "Crypto.Cipher.AES",
        "--hidden-import", "Crypto.Hash.HMAC",
        "--hidden-import", "Crypto.Protocol.KDF",
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
        print(f"\n打包成功：{exe}  ({exe.stat().st_size / 1024 / 1024:.1f} MB)")
        print("验证：dist\\TraeCollector.exe --auto --dry-run  (静默, EXIT=0)")
        print("部署：复制到 assets\\TokenCollector.exe 后重新 cargo build --release")
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
