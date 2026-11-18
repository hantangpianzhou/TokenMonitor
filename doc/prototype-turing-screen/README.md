# 图灵智显 3.5 寸小屏 — 无硬件原型

**结论先行**：悬浮窗完整画面投屏在这块屏上不可行（CH552T 无帧缓冲、整帧刷新 1.5–2 秒，见评审记录）。本原型走的是可行路线——**小屏显示 TokenMonitor 的 token 数据/状态**，屏端 MCU 自绘，主机只发数据。

本目录的代码**不需要屏幕即可运行**，用于屏到手之前把 UI、数据流、RGB565 编码全部定下来。

## 文件清单

| 文件 | 作用 |
|---|---|
| `screen_loop.py` | **常驻推送程序（exe 打包入口）**：读库→渲染→发串口，定时轮询 + 变化检测 + 断链重试 + 手动刷新 |
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
$PY screen_loop.py --demo --dry-run --once        # 无库无硬件自测一帧
$PY screen_loop.py --list                          # 找 1a86:5722 串口
$PY screen_loop.py --port COM3                     # 常驻推送（省略 --port 则自动探测）
```

## 打包 exe（方案1：PyInstaller，目标机零 Python）

```bash
$PY build_exe.py --check          # 检查环境（PyInstaller / pyserial / Pillow）
$PY build_exe.py --clean          # 清理旧构建 + 打包 → dist/TokenScreen.exe
# 或双击「一键打包.bat」
```

产物 `dist/TokenScreen.exe`（单文件约 17 MB），已内置 Python 解释器 + Pillow + pyserial + 渲染/协议模块。

**目标机部署**：
1. 把 `TokenScreen.exe` 拷到目标机（无需装 Python）
2. 目标机须同时运行 TokenMonitor（Rust exe）——数据来自同机 `%APPDATA%/TokenMonitor/tokenmonitor.db`
3. 插上图灵屏，exe 自动按 VID:PID `1a86:5722` 探测串口；也可 `--port COM3` 指定
4. 双击 exe 即常驻推送；关掉窗口停止

**exe 常用参数**：`--interval 60`（轮询秒）、`--orient landscape/portrait`、`--brightness 25`（0 最亮）、`--demo --dry-run --once`（自检，不写串口）。屏上时间每 5 分钟兜底刷新一次；手动建 `%TEMP%/tm-screen-refresh.trigger` 文件可立即触发一帧。

**移植性**：字体改为多路径自适应（msyh→simhei→simsun→思源→PIL 默认），英文版 Windows 不崩；控制台启动切 UTF-8，中文日志不乱码。已实测 exe 在无 Python 环境独立运行、读真实库、枚举串口。

## 集成进 TokenMonitor 主程序（2026-11-17）

数显功能已内嵌进桌面主程序，**分发只需一个 exe**。相关 Rust 代码：

| 文件 | 作用 |
|---|---|
| `src/app/screen_push.rs` | 内嵌 `assets/TokenScreen.exe`（17 MB），首次释放到 `%TEMP%/tokenmonitor-screen/` 并复用；`--once` 短命进程推一帧后自退；防重入 + `CREATE_NO_WINDOW` 无黑框 + `Drop` 退出兜底 |
| `src/app/app.rs` | `ScanCompleted`（一轮全量扫描的最后一个 provider）触发推送；开关 setter |
| `src/collector/mod.rs` | 开关持久化 key `screen.push.enabled`（默认关） |
| `src/ui/settings/page.rs` | 设置 → 通用 →「数显推送」开关（照抄开机自启） |
| `assets/TokenScreen.exe` | 内嵌的打包产物；**改了 Python 脚本后须重跑 `build_exe.py` 并覆盖此文件，再重新编译 Rust** |

**数据流**：

```
TokenMonitor 扫描（5 分钟 / 手动刷新）
   └─ ScanCompleted（最后一个 provider）
        └─ 开关开？ → spawn 内嵌 TokenScreen.exe --once（读同一 SQLite）
                        └─ 渲染+RGB565+串口推一帧 → 自退
```

- **频率 = 检测频率**：每次完整扫描推一帧（含手动刷新，走同一路径）；数据未变也推，用于刷新屏上时钟。
- **无常驻进程**：子进程推完自退，主程序只在 `Drop` 时 kill 残留，无生命周期轮询。
- **feature 门控**：`screen_push` 随 `app` 模块仅在 `ui` feature 编译；TUI 构建不含 17 MB 载荷。
- **重打包流程**：改 Python → `build_exe.py --clean` → 把 `dist/TokenScreen.exe` 覆盖到 `assets/` → `cargo build --release`。
- **验证**：`cargo check --all-targets`、`cargo test --lib`（206 项）、TUI 构建、`cargo fmt --check` 全通过；释放出的 exe 实测 `--dry-run --once` 读真实库正常。

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

## 显示链路（屏到手按顺序做）

1. 收货先验型号（上表），确认是 Revision A。
2. 插 USB-C，设备管理器出现 `USB-SERIAL CH552` 的 COMx（不是显示器！）；`--list` 找 `1a86:5722`。
3. `python configure.py`（官方库）设 `REVISION: A`、`COM_PORT: AUTO`，跑官方 system monitor 确认屏能亮——**先用官方软件排除硬件问题**。
4. 官方软件通过后再用本脚本：`--self-test` → `--port COM3 --frame frame_landscape.rgb565 --orient landscape`。
5. 画面方向若歪 90°，把 `send_to_screen.py` 里 `rotate_cw` 换成 `rotate_ccw` 重发。

## 协议核对来源

`mathoudebine/turing-smart-screen-python` 官方源码：`library/lcd/lcd_comm_rev_a.py`（命令码/坐标拆分/16 字节方向包）、`library/lcd/serialize.py`（`image_to_RGB565(..., "little")`、分块 `width*8`）。自检断言按源码逐字节复刻。

## 屏幕信息架构（2026-11-15 修订）

去掉了「近 7 日趋势图」（小屏上信息价值低、且对比度不达标），底部改为**工作环境区**：

| 区域 | 内容 | 数据来源 |
|---|---|---|
| 标题栏 | TOKEN MONITOR + 日期 | 真实时钟 |
| 用量/费用双卡 | 今日 token、今日费用、调用条数 | 真实 SQLite |
| Agent 分布 | 占比最高的来源 + 比例条 | 真实 SQLite |
| 系统负载 | CPU / 内存 / 磁盘 三条 + 比例条 | **Windows 原生 API 真实采集**（ctypes，零依赖，无需 psutil） |
| 工作时间 | 时钟 + 天气 | 时钟真实；**天气是样例值，图上标 `DEMO`**，未接 API |

横屏 = 负载在左、时钟天气在右；竖屏 = 上下分区。

**天气接真实数据**：`render_screen.py` 的 `collect_env()` 里把 `weather` 换成 API 返回值，`weather_demo` 改为 `False` 即可去掉 DEMO 标记。也可用 `--json` 注入完整 `env`（缺字段有 `default_env()` 兜底，不会崩）。

## 数据来源（真实库）

- 库路径：`%APPDATA%/TokenMonitor/tokenmonitor.db`（只读打开，不写入、不加锁）
- 表：`usage_records`，字段 `input/output/cache_read/cache_write/cost_micros/started_at`
- 今日窗口：`started_at` 落在东八区当天 `[00:00, now)`（用 UTC 边界写入 SQL）
- 系统负载：`GetSystemTimes` 两次采样算 CPU、`GlobalMemoryStatusEx` 取内存、`GetDiskFreeSpaceExW` 取 C 盘，均零依赖
- 数值是运行当时的快照，会随时间变化（已实测从 1.4百万/$0.09/17条 涨到 1.5千万/$0.66/114条）

## 已验证 / 未验证

| 项 | 等级 | 状态 |
|---|---|---|
| 真实 SQLite 读取与东八区日界聚合 | 局部实测 | 通过，数字与库一致 |
| 排版：双卡/Agent/系统负载/时钟天气无重叠溢出 | 局部实测 | 通过，放大后逐张目视核对，修过竖屏底部时钟裁切 |
| Windows 原生 API 采集 CPU/内存/磁盘 | 局部实测 | 通过，CPU 两次采样、内存 `dwMemoryLoad`、磁盘 `GetDiskFreeSpaceExW` 均返回真实值 |
| RGB565 小端编码 + 往返解码一致 | 局部实测 | 通过，307,200 字节，解码图与原图一致 |
| 字体（微软雅黑 msyh.ttc/msyhbd.ttc）在当前机器可用 | 局部实测 | 通过 |
| Revision A 命令帧/坐标拆分/方向包/分块 | 局部实测 | 通过，`--self-test` 按官方源码逐字节断言 |
| 横屏帧旋转重排可逆且方向自洽 | 局部实测 | 通过，物理排列图文字顺时针躺 90°，与 SET_ORIENTATION(LANDSCAPE) 抵消 |
| 屏端是否真按协议点亮、方向是否需 cw/ccw 切换 | 未证实 | 屏未到手，待 L1 单点验证 |
| 买到的是不是 Revision A（而非玄方 B） | 未证实 | 收货后按上表验机 |

## 屏到手后的接入步骤

1. 用 `lsusb` / 设备管理器确认主控：CH552T 应显示 `VID:PID 1a86:5722`；玄方是 HID、FTDI 老版协议不同，均不支持本方案。
2. 跑 `python configure.py`（`mathoudebine/turing-smart-screen-python`）确认识别，记下 `REVISION`。
3. 用本原型导出 `frame_landscape.rgb565` 作为 L1 测试帧，整帧发一次，确认画面与 `preview_landscape.png` 一致（这一步同时验证字节序）。
4. 通过后，把 `render` 函数的数据源从 SQLite 换成 TokenMonitor 定时导出的 JSON（`--json` 入口已备好），按刷新间隔下发。

**绑定版本**：Python 3.13.12 / Pillow 13.0.0 / Windows 字体 msyh.ttc。换机器需重验字体路径。

**失效触发条件**：换屏型号或 revision、换主控（非 CH552T）、屏固件更新、TokenMonitor 改 `usage_records` 表结构。
