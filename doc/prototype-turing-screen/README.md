# 图灵智显 3.5 寸小屏推送

**结论先行**：悬浮窗完整画面投屏在这块屏上不可行（无帧缓冲、整帧刷新 1.5–2 秒）。可行路线是——**PC 端渲染整帧，通过 USB CDC 串口把 RGB565 图像推到屏**。该功能已集成进 TokenMonitor 桌面主程序（设置 →「数显推送」开关），分发只需一个 MSI。

本目录保留原型/诊断脚本：即使没有屏幕，`--demo --dry-run` 也能自测渲染、RGB565 编码与协议帧。当前实现为**常驻进程 + 脏矩形局部刷新**（首次全帧，之后时钟每分钟/数据变化只推约 4.6KB 局部，每 10 分钟全帧校准一次重建基准），详见下文集成段。

## 文件清单

| 文件 | 作用 |
|---|---|
| `screen_loop.py` | **推送程序（exe 打包入口）**：读库→渲染→发串口。两种模式：`--partial` 常驻+脏矩形局部刷新（主程序用）、`--once` 短命全帧（单帧诊断用）；含变化检测、断链重试、手动刷新 |
| `render_screen.py` | 渲染：读真实 SQLite + 时钟 → 480×320 / 320×480 → PNG + RGB565；字体多路径自适应 |
| `send_to_screen.py` | **发送脚本**：官方协议 Revision A，命令帧/方向/亮度/分块/旋转重排，含 `--self-test` |
| `build_exe.py` | **打包脚本（方案1）**：PyInstaller 打成单文件 `dist/TokenScreen.exe`，含环境检查 |
| `一键打包.bat` | 双击打包的 bat 包装（自动定位本机 Python） |
| `verify_rgb565.py` | 往返验证：把 .bin 解码回 PNG，确认字节序 |
| `verify_rotation.py` | 验证横屏帧旋转重排后方向正确 |
| `preview_landscape.png` / `preview_portrait.png` | 原始预览（真实数据） |
| `preview_*_2x.png` | 2 倍放大，便于看排版 |
| `frame_landscape.rgb565` / `frame_portrait.rgb565` | 整帧像素，各 307,200 字节 |
| `verify_roundtrip.png` | 从 .bin 解码回来的图，用于核对字节序 |

## 运行

```bash
PY=C:/Users/Lenovo/.workbuddy/binaries/python/versions/3.13.12/python.exe

$PY render_screen.py --emit-rgb565                # 真实数据，横屏
$PY render_screen.py --orient portrait --emit-rgb565   # 竖屏
$PY render_screen.py --demo                       # 不连数据库，用样例数据
$PY render_screen.py --json data.json             # 外部注入聚合结果
$PY verify_rgb565.py                              # 验证字节序
$PY send_to_screen.py --self-test                 # 协议自检（无需硬件）
$PY send_to_screen.py --list                      # 列出串口，找 1a86:5722
$PY send_to_screen.py --port COM3 --frame frame_landscape.rgb565 --orient landscape

# 常驻推送程序（exe 入口）
$PY screen_loop.py --demo --dry-run --once        # 无库无硬件自测一帧（全帧模式）
$PY screen_loop.py --list                          # 找 1a86:5722 串口
$PY screen_loop.py --partial --interval 15         # 常驻局部刷新（主程序用：首次全帧，之后只推脏矩形）
$PY screen_loop.py --partial --demo --dry-run --once  # 局部刷新逻辑自测，不写串口
```

## 打包 exe（方案1：PyInstaller，目标机零 Python）

```bash
$PY build_exe.py --check          # 检查环境（PyInstaller / pyserial / Pillow）
$PY build_exe.py                  # 打包 → dist/TokenScreen.exe（PyInstaller 自带 --clean）
# 或双击「一键打包.bat」
# 注意：不要传 --clean，外层 shutil.rmtree 在沙箱里会被 safe-delete 拦截报错
```

产物 `dist/TokenScreen.exe`（单文件约 17 MB），已内置 Python 解释器 + Pillow + pyserial + 渲染/协议模块。

**目标机部署**：
1. 把 `TokenScreen.exe` 拷到目标机（无需装 Python）
2. 目标机须同时运行 TokenMonitor（Rust exe）——数据来自同机 `%APPDATA%/TokenMonitor/tokenmonitor.db`
3. 插上图灵屏，exe 自动按 VID:PID `1a86:5722` 探测串口；也可 `--port COM3` 指定
4. 双击 exe 即常驻推送；关掉窗口停止

**exe 常用参数**：`--interval 60`（轮询秒）、`--orient landscape/portrait`、`--brightness 50`（0–100 百分比，**100 最亮 / 0 最暗**，默认 50）、`--demo --dry-run --once`（自检，不写串口）、`--partial`（常驻局部刷新，见下）、`--full-calibrate-sec 600`（局部模式全帧校准间隔秒，默认 600=10 分钟；0=禁用，见下）。
- **全帧模式**（默认，单帧诊断用）：每次推整帧 307200B，含 RESET，约 12s。
- **局部模式**（`--partial`，主程序用）：首次 RESET + 全帧建基准，之后串口常开；时钟每分钟 / token 数据变化 / 手动触发只推脏矩形（时钟区约 4.6KB、全帧的 1.5%、无 RESET、约 0.3s）；**每 10 分钟强制全帧校准一次**（`--full-calibrate-sec 600`，重建 last_frame 基准，自愈屏幕与基准的失步）。手动建 `%TEMP%/tm-screen-refresh.trigger` 可立即触发一帧。

**移植性**：字体改为多路径自适应（msyh→simhei→simsun→思源→PIL 默认），英文版 Windows 不崩；控制台启动切 UTF-8，中文日志不乱码。已实测 exe 在无 Python 环境独立运行、读真实库、枚举串口。

## 集成进 TokenMonitor 主程序（2026-11-19 定稿）

数显功能已内嵌进桌面主程序，**分发只需一个 exe**。相关 Rust 代码：

| 文件 | 作用 |
|---|---|
| `src/app/screen_push.rs` | 内嵌 `assets/TokenScreen.exe`（17 MB），首次释放到 `%TEMP%/tokenmonitor-screen/` 并复用；管理一个**常驻** `--partial` 进程：`ensure_running()` 启动、`notify()` 写触发文件、`stop()` 释放 COM3；`CREATE_NO_WINDOW` 无黑框 + `Drop` 退出兜底 |
| `src/app/app.rs` | 构造后若开关已开则拉起常驻进程；`set_screen_push` 开/关对应启停；扫描完成（含手动刷新）写触发文件通知局部刷新 |
| `src/collector/mod.rs` | 开关持久化 key `screen.push.enabled`（默认关） |
| `src/ui/settings/page.rs` | 设置 → 通用 →「数显推送」开关（照抄开机自启） |
| `assets/TokenScreen.exe` | 内嵌的打包产物；**改了 Python 脚本后须重跑 `build_exe.py` 并覆盖此文件，再重新编译 Rust** |

**数据流（常驻 + 脏矩形局部刷新，2026-11-19 定稿）**：

```
主程序启动（开关已开）/ 打开开关
   └─ ensure_running() → spawn 内嵌 TokenScreen.exe --partial（常驻，串口常开）
        └─ 首次：RESET → 重枚举 → 全帧 307200B 建基准（唯一一次 RESET）
   ├─ 常驻进程自己每 15s 轮询：时钟跨分钟 / token 数据变化 → 推脏矩形局部帧
   ├─ 每 10 分钟：强制全帧 307200B 校准一次（不 RESET），重建 last_frame 基准
   └─ 主程序每轮扫描完成 → notify() 写 %TEMP%/tm-screen-refresh.trigger
        └─ 常驻进程检测到 → 立即推一帧局部刷新（读同一 SQLite）
关闭开关 / 程序退出 → stop() kill 常驻进程，释放 COM3
```

- **为何必须常驻**：这块屏只有串口保持常开时局部帧才被锁存；短命进程推完关串口 → 屏幕复位回待机，下一帧局部数据打在空屏上（实机验证）。
- **刷新成本**：首次全帧约 12s（含 RESET 重枚举）；之后时钟区每次约 4.6KB（全帧的 1.5%）、约 0.3s、无 RESET 无黑屏；每 10 分钟一次全帧 307200B（约 0.8–2s）用于自愈失步。
- **时钟 = 每分钟**：由常驻进程自己驱动，**不再依赖扫描周期**（旧版每 5 分钟才刷一次时钟）。
- **为何要全帧校准**：脏矩形以内存 `last_frame` 为基准，若局部帧丢包或屏幕自复位导致显示与基准失步，数据不变时永远刷不回来；定期全帧覆盖整屏并重建基准，把失步自愈周期压到 10 分钟以内。
- **feature 门控**：`screen_push` 随 `app` 模块仅在 `ui` feature 编译；TUI 构建不含 17 MB 载荷。
- **重打包流程**：改 Python → `python build_exe.py`（PyInstaller 自带 `--clean`；直接跑，**不要传 `--clean`**——外层 `build_exe.py --clean` 的 `shutil.rmtree` 在沙箱里会被 safe-delete 拦截报 OSError）→ 把 `dist/TokenScreen.exe` 覆盖到 `assets/` → `cargo build --release`。
- **验证**：`cargo check`、`cargo test --lib`（211 项）、`cargo fmt` 全通过；新 exe 以主程序同款参数 `--partial --interval 15` 实机跑通（全帧建基准 + 跨分钟局部刷新 4614B）。

## Revision A vs B — 买屏/收货必看（官方 wiki 核对）

| 判据 | Revision A（图灵 TURZX 3.5"，本方案支持） | Revision B（玄方 XuanFang，本脚本**不支持**） |
|---|---|---|
| 随机 Windows 软件 | `UsbMonitor.exe` | `ExtendScreen.exe` |
| 串口序列号 | `USB35INCHIPS` / `USB35INCHIPSV2` | `2017-2-25` |
| 背面标识 | "Turing Smart Screen" 中英文 | `Q002_V2.0` |
| USB-C 口 | 同侧两个 | 分列两侧 + JST 主板接口 |
| 协议 | 6 字节命令帧，RGB565 **小端**，115200 | 10 字节帧（首尾命令），RGB565 **大端** |
| HELLO(0x45×6) | 官方图灵不应答，属正常 → 判 320×480 | 应答 10 字节，含子版本 A01/A02/A11/A12 |

**卖家标签不可信**：官方 wiki 明说 Turing 3.5" 常被错标为 revision A；判别以随机软件和背面丝印为准。本原型的小端导出**匹配 Revision A**；若买成玄方 Revision B，需改 `send_to_screen.py` 的字节序和命令帧，或直接用官方库 `config.yaml` 设 `REVISION: B`。

## 显示链路（验机/点亮的推荐顺序，已实测通过）

1. 收货先验型号（上表），确认是 Revision A。
2. 插 USB-C，设备管理器出现 `USB-SERIAL CH552` 的 COMx（不是显示器！）；`--list` 找 `1a86:5722`。
3. `python configure.py`（官方库）设 `REVISION: A`、`COM_PORT: AUTO`，跑官方 system monitor 确认屏能亮——**先用官方软件排除硬件问题**。
4. 官方软件通过后再用本脚本：`--self-test` → `--port COM3 --frame frame_landscape.rgb565 --orient landscape`。
5. 画面方向若歪 90°，把 `send_to_screen.py` 里 `rotate_cw` 换成 `rotate_ccw` 重发。

## 协议核对来源

`mathoudebine/turing-smart-screen-python` 官方源码：`library/lcd/lcd_comm_rev_a.py`（命令码/坐标拆分/16 字节方向包）、`library/lcd/serialize.py`（`image_to_RGB565(..., "little")`、分块 `width*8`）。自检断言按源码逐字节复刻。

## 屏幕信息架构（2026-11-19 定稿）

只显示时间与 token 数据；近 7 日趋势、金额/费用、天气、CPU/内存/磁盘均已去掉。横屏 480×320，底部 2:1 分区（AGENT 占 2/3、时间占 1/3）：

| 区域 | 内容 | 数据来源 |
|---|---|---|
| 标题栏 | TOKEN MONITOR + live 点 + 日期 | 真实时钟 |
| 用量/活跃双卡 | 今日 token、今日调用次数 + 最近活动时间 | 真实 SQLite |
| Agent 分布 | top 3 来源名 + 占比 + 比例条（provider 名经 `PROVIDER_CN` 映射，如 `teleagent`→`TeleAgent`） | 真实 SQLite |
| 当前时间 | 大时钟，分钟级即时刷新，垂直居中 | 真实时钟 |

竖屏为上下分区，同样只含 token + 时间。

## 数据来源（真实库）

- 库路径：`%APPDATA%/TokenMonitor/tokenmonitor.db`（只读打开，不写入、不加锁）
- 表：`usage_records`，字段 `input/output/cache_read/cache_write/cost_micros/started_at`
- 今日窗口：`started_at` 落在东八区当天 `[00:00, now)`（用 UTC 边界写入 SQL）
- 数值是运行当时的快照，随时间变化（token / 次数 / 最近活动时间）

## 已验证 / 未验证

| 项 | 等级 | 状态 |
|---|---|---|
| 真实 SQLite 读取与东八区日界聚合 | 局部实测 | 通过，数字与库一致 |
| 排版：双卡/Agent/大时钟无重叠溢出、时钟垂直居中 | 局部实测 | 通过，放大后逐张目视核对 |
| RGB565 小端编码 + 往返解码一致 | 局部实测 | 通过，307,200 字节，解码图与原图一致 |
| 字体（微软雅黑 msyh.ttc/msyhbd.ttc）在当前机器可用 | 局部实测 | 通过 |
| Revision A 命令帧/坐标拆分/方向包/分块 | 局部实测 | 通过，`--self-test` 按官方源码逐字节断言 |
| 横屏帧 180° 校正 + 320 列重排方向正确 | 实机 | 通过，用户确认屏幕方向正确 |
| 屏端按协议点亮、RESET→重枚举→重开序列 | 实机 | 通过，纯红/彩虹/真实界面均成功点亮 |
| 局部刷新（串口常开、脏矩形、无 RESET） | 实机 | 通过，时钟跨分钟只推 4614B，屏幕即时更新无黑屏、无残影 |
| 周期全帧校准（每 N 秒重建 last_frame 基准） | 实测 | 通过，dry-run 3s 与真机 10s 间隔均按点触发全帧、无 RESET、无黑屏，期间时钟局部刷新照常 |
| 买到的是 Revision A（VID:PID 1a86:5722 / USB35INCHIPSV2） | 实机 | 已确认 |

## 屏端排障要点（实机总结）

1. **选口**：设备管理器 / `--list` 确认 `VID:PID 1a86:5722`（本机 COM3）。新版屏用通用 CDC 驱动，描述可能只显示「USB 串行设备」，按描述关键词或唯一串口兜底。
2. **点亮序列**：打开串口后 `sleep(1)` → 发 RESET → `sleep(2)` → 关串口等重枚举 4s → 重开并清缓冲 → 分开发 SCREEN_ON/亮度/方向/位图头（各隔 0.3s）→ 逐块写像素 → 末尾 `sleep(1)`。任一步抢时序都会卡在 `please run the app` 待机画面（曾据此排查多轮）。
3. **局部刷新前提**：串口必须保持常开。进程退出关串口 → 屏幕复位 → 局部帧打在空屏上。
4. **不出画面时**：先用彩虹纯色块脚本排除内容/字体问题；再对比成功脚本与失败程序的逐行时序差异，别先猜硬件或流控。

**绑定版本**：Python 3.13.12 / Pillow 13.0.0 / Windows 字体 msyh.ttc。换机器需重验字体路径。

**失效触发条件**：换屏型号或 revision、换主控（非 CH552T）、屏固件更新、TokenMonitor 改 `usage_records` 表结构。
