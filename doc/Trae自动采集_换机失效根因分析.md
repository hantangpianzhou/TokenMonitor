# Trae 自动采集换机失效：根因分析

> 日期：2026-11-19 · 触发：v0.3.13 安装包装到别的电脑后，没有动态解析出 Trae 用量
> 别机环境（用户确认）：中文版 Trae CN、打开时 Trae 开着且刚跑过 agent 任务、用的是本机刚打的新包
> 结论先行：**本机"能采集"是因为缓存 key 一直有效、走的是快路径；"无缓存→扫内存提 key"这条新路径本机从未真实跑过。对照实验证明该路径存在两个确凿 bug，导致换机后提 key 失败时既不写冷却、又不留任何日志，每轮扫描还空耗 60 秒。**

---

## 一、本机为什么"一直正常"

```
本机: 缓存 key (trae_key.json) 存在且 HMAC 校验通过
      └─> --auto 直接复用缓存 → 解密 → 入库   （从未扫内存）
```

本机从 11-17 手工写入缓存 key 后，Trae 主库 salt 未变，HMAC 一直校验通过。也就是说 11-19 新加的"自动提 key"代码（commit `3587441`）在本机**从未被执行过**。换机后缓存不存在（且 key 与本机 salt 绑定，拷过去也无效），才第一次真正进入这条未经验证的路径。

## 二、对照实验（本机，移走缓存 key 后跑打包 exe）

```
操作: 移走 %APPDATA%\TokenMonitor\trae_key.json
      → 直接运行 assets\TokenCollector.exe --auto --dry-run
结果:
  RC = 6        耗时 107.5s
  stdout: "[key] 目标进程 25 个，开始扫描内存..." → "扫描超过 60s 上限"
  stderr: "[auto] Trae 采集跳过: 'gbk' codec can't encode character '\u2717'..."
  冷却文件: 未生成（no cooldown file）
  key: 未重新生成
```

实验同时命中了两个 bug。

## 三、根因链（两个确凿 bug + 一个设计缺陷）

```
换机 → 无缓存 key
   └─> --auto 触发自动提 key (trae_collector.py:504)
        └─> 扫 25 个进程内存，60s 上限 (:278)  ← BUG1: 时间不够，实测超时
             └─> 失败 → get_key 里 log("✗ ...") (:325)
                         └─> BUG2: '✗'(U+2717) 在 GBK stdout 无法编码 → UnicodeEncodeError
                              └─> 冒泡到 main 的 except Exception (:520)
                                   ├─> mark_extract_failed(:506) 没执行 → 冷却不写
                                   └─> 错误写 stderr → 被主程序 Stdio::null() 丢弃 (trae_ingest.rs:96)
                                        └─> BUG3: 用户完全看不到任何原因
```

### BUG 1 — 提 key 扫描 60s 上限不够（正确性）

`trae_collector.py:278`：

```python
if time.perf_counter() - t0 > MAX_SCAN_SECONDS:   # 60
    log("[key] 扫描超过 %ds 上限" % MAX_SCAN_SECONDS)
    break
```

本机实测 25 个 Trae/TeleAgent 进程、内存合计数十 GB，60s 扫不完就中断。key 若不在扫描到的前几个进程里就会被漏掉。用户虽"刚跑过 agent 任务"，但 key 具体落在哪个进程、是否已被换出，都不确定。

### BUG 2 — `✗` 字符导致 GBK 编码崩溃，冷却机制整体失效（致命）

`trae_collector.py:325`：

```python
log("✗ 提 key 失败：请先发起一次 agent 任务再重试")
```

- 本机 shell 有 `PYTHONIOENCODING=utf-8`，所以手动跑源码不崩；
- 但主程序拉起的是 PyInstaller 打包的 exe，干净环境下 Windows 中文 locale 的 stdout 是 **GBK**；
- 中文 GBK 能编码，但 `✗`（U+2717）GBK **不能编码** → `UnicodeEncodeError`；
- 该异常不是 `ExtractError`，直接冒泡到 `main` 的 `except Exception`（`:520`），于是：
  - `:506` 的 `mark_extract_failed()` **永远执行不到** → 30 分钟冷却文件不写；
  - 本该 30 分钟才重试一次，变成**每 5 分钟扫描都空转 60s**；
  - 错误只写 stderr，又被 `trae_ingest.rs:96` 的 `Stdio::null()` 丢弃。

这解释了"换机后既没数据、又没有任何提示、程序还变慢"的完整现象。

### BUG 3 — 诊断信息全部不可见（可观测性）

`trae_ingest.rs:94-97`：

```rust
cmd.arg("--auto")
    .stdin(Stdio::null())
    .stdout(Stdio::null())
    .stderr(Stdio::null());   // 子进程所有输出被丢弃
```

采集器是静默短命进程，唯一排障出口（stderr）被丢。换机出问题时用户和开发者都无从得知卡在哪一步。

### 潜在 BUG 4 — 国际版进程名缺失

目录检测兼容国际版 `Trae`（`:51`），但进程匹配 `PROC_HINTS`（`:71`）只有 `trae cn.exe / teleagent.exe / ais.exe`，**没有 `Trae.exe`**。本机场景（中文版）不触发，但国际版机器会一个 PID 都找不到。（本次用户已确认是中文版，非本次主因，但应一并修。）

---

## 四、如何在那台电脑上确认（不必改代码）

按下面顺序跑，看卡在哪一环：

```bat
:: 1) 看缓存 key 和冷却文件是否存在
dir "%APPDATA%\TokenMonitor\trae_key.json"
dir "%APPDATA%\TokenMonitor\trae_key_fail.json"

:: 2) 直接跑打包 exe，把输出留在屏幕上（关键：设 UTF-8 避免编码崩溃掩盖真相）
set PYTHONIOENCODING=utf-8
cd /d "<TokenMonitor 安装目录>"
TokenCollector.exe --auto --dry-run
echo EXIT=%ERRORLEVEL%
```

判读：

| 现象 | 说明 |
|---|---|
| 无 `trae_key.json` | 符合预期（换机无缓存），必须走扫内存提 key |
| 输出"扫描超过 60s 上限" | 命中 BUG1，60s 不够 |
| 输出 `'gbk' codec can't encode` | 命中 BUG2（未设 PYTHONIOENCODING 时） |
| `EXIT=6` | 走了 except 异常分支，冷却没写 |
| `trae_key_fail.json` 存在 | 冷却正常写入（当前 bug 下通常**不会**有） |
| 输出"命中 ... 提取成功" | 提 key 成功，问题在解密/入库环节，另查 |

## 五、修复方案（待确认后实施）

按优先级，全部集中在 Python 采集器 + 一处 Rust：

| # | 改动 | 文件 | 说明 |
|---|---|---|---|
| 1 | `log()` 编码安全：启动即 `sys.stdout.reconfigure(errors="replace")` 或把所有 `✗/→` 换成 ASCII | `trae_collector.py` | 根治 BUG2，保证失败路径不崩 |
| 2 | 调整顺序：**先写冷却再 log**，并把 `mark_extract_failed` 移进 `get_key` 失败分支内 | `trae_collector.py` | 冷却必写，避免每轮空转 60s |
| 3 | 扫描超时 60s → 180s，且 Trae CN.exe 优先扫描 | `trae_collector.py` | 提高命中率 |
| 4 | 关键状态写文件日志 `%APPDATA%\TokenMonitor\trae_ingest.log`（不依赖 stdout） | `trae_collector.py` | 根治 BUG3，换机可排查 |
| 5 | `PROC_HINTS` 增加 `"trae.exe"` | `trae_collector.py` | 兼容国际版 |
| 6 | 主程序不再丢弃 stderr（或采集器自行落盘即可，此项可选） | `trae_ingest.rs` | 配合 #4 |

改完后需：`python scripts\build_trae_exe.py --clean` → 复制 `scripts\dist\TraeCollector.exe` 到 `assets\TokenCollector.exe` → `cargo build --release` → 重打 MSI。

> 关键提醒：修复必须重新打包 **exe 并重新编译主程序**（exe 是 `include_bytes!` 编进去的），只改 Python 源码对已安装的 0.3.13 无效。

---

## 六、证据与边界

- 根因来自真实代码审查（行号已标注）+ 本机对照实验（移走缓存 key 跑打包 exe），非推测。
- 编码结论双重确认：本机 shell `PYTHONIOENCODING=utf-8` 所以手动跑不崩；打包 exe 干净环境走 GBK，`✗` 无法编码。
- 未能直接访问用户那台电脑，"刚跑过 agent 任务时 key 是否仍驻留内存"无法实测；BUG1/BUG2 已能独立解释"无数据 + 无提示 + 变慢"的现象。
- 本次未修改任何代码，仅产出分析文档；实施前请确认第五节方案。

---

## 七、修复实施记录（2026-11-19 晚，已落地，版本 0.3.14）

用户确认「升到 0.3.14 + 全部 6 项」，已实施并端到端验证。

### 实际改动

| # | 改动 | 文件 / 位置 |
|---|---|---|
| 1 | `log()` 启动即 `sys.stdout.reconfigure(utf-8, errors=replace)` + print 失败再降级 ASCII；所有 `✗` 换 `[X]` | `trae_collector.py` `_fix_stdio()`/`log()` |
| 2 | 提 key 失败路径先 `mark_extract_failed` 再返回；`--auto` 的 `except Exception` 也补写冷却 | `trae_collector.py` `main()` |
| 3 | 扫描上限 60s→180s；**内层内存块循环也查超时**（原来只在 pid 循环开头查，单个大进程读完就超时）；Trae 主进程优先扫 | `trae_collector.py` `extract_key_from_memory()`/`_target_pids()` |
| 4 | 每次运行全程落盘 `%APPDATA%\TokenMonitor\trae_ingest.log`（UTF-8，超 1MB 轮转为 .1） | `trae_collector.py` `log()` |
| 5 | `PROC_HINTS` 增加 `"trae.exe"`（国际版） | `trae_collector.py:71` |
| 6 | 主程序注释更新为新行为（不再写"绝不扫内存"） | `trae_ingest.rs:8-9` |
| — | 版本号 0.3.13→0.3.14 | `Cargo.toml` / `Cargo.lock` / `.temp/build_msi.py:33` |

### 验证结果（移走缓存 key 跑新打包 exe）

| 项目 | 修复前 | 修复后 |
|---|---|---|
| 退出码 | RC=6（UnicodeEncodeError 崩溃） | **RC=2**（正常失败路径）|
| `✗` 编码 | `'gbk' codec can't encode` | **无**，中文正常 |
| 冷却文件 | 未生成 | **已生成**（30 分钟退避生效）|
| 日志落盘 | 无 | **trae_ingest.log 全程 UTF-8 记录** |
| 扫描上限 | 60s（外层才查） | 180s（内外层都查）|

> 本次测试本机无 agent 任务在跑，key 不在内存，故 180s 仍未提到——符合预期。修复的核心是**失败时优雅退出 + 写冷却 + 留日志**，而非崩溃空转。换机且 Trae 近期有 agent 任务时，走同一条路径可命中 key 并自愈。

### 部署链（已完成）

```
scripts/trae_collector.py 改
  → python scripts/build_trae_exe.py --clean   （新 exe 11.3MB）
  → 复制 scripts/dist/TraeCollector.exe → assets/TokenCollector.exe（MD5 一致已校验）
  → cargo build --release                        （include_bytes! 编进主程序）
  → python .temp/build_msi.py                    （出 TokenMonitor-v0.3.14-windows-x64.msi）
```

---

## 第八节：0.3.15 再修 —— 提 key 路径本身的两个致命 bug（2026-11-23）

### 为什么 0.3.14 仍不能算"解决"

0.3.14 只修了"失败要优雅"的 6 项，但**提 key 这条路径本身从未端到端成功过**。从对话记录还原出本机唯一一次真实提 key 成功的日志：

```text
target PIDs: 12
PID 31768  Trae CN.exe  scanned  294MB  saltAnchors=0    28s
PID 2904   Trae CN.exe  scanned  315MB  saltAnchors=0    57s
PID 26776  Trae CN.exe  scanned 1189MB  saltAnchors=0   167s
PID 17944  Trae CN.exe  scanned  415MB  saltAnchors=0   207s
PID 32500  Trae CN.exe  scanned  451MB  saltAnchors=0   251s
*** FOUND key=...  tag=x96  PID 11636
PID 11636  Trae CN.exe  scanned   34MB  saltAnchors=15  254s
```

两个此前没发现的致命问题：

| # | Bug | 证据 | 后果 |
|---|---|---|---|
| 1 | **进程扫描顺序错误** | key 在仅 34MB 的 PID 11636，却排 tasklist 第 6；前 5 个大进程（合计 2.6GB）白扫 251s；累计命中 254s > `MAX_SCAN_SECONDS=180` | 即使 Trae 活跃、key 在内存，也**必然超时**，换机永远提不到 |
| 2 | **预筛误丢 x96 命中块** | 成功 tag=`x96`（key 是 `x'<64hex>'` 文本），但预筛 `if salt in chunk` 只保留含 **binary salt** 的块 | 若该内存块恰好无 binary salt（本机这次有 15 个是运气），含 key 的块被直接丢弃 |

### 修复（0.3.15）

| 改动 | 位置 | 说明 |
|---|---|---|
| 进程按工作集升序扫描 | `_target_pids()` | 一次 tasklist 同时取 PID + Mem Usage，`primary`/`others` 各自 `sort(key=内存KB)`，小进程先扫 ⇒ 34MB 的命中进程从"第 6 位/254s"变"第 1 位/秒级" |
| 预筛放宽 | `extract_key_from_memory()` | `if salt in chunk or b"x'" in chunk` —— binary salt 与 x96 文本两种锚点都保留 |
| 全局 X96 正则 | 模块级 `X96_RE` | `_scan_regions` 与预筛共用，避免正则重复定义 |

### 单元验证（不扫真实内存，构造块）

| 验证项 | 结果 |
|---|---|
| x96-only 块（无 binary salt）能被 `_scan_regions` 命中 | ✅ |
| 旧预筛会丢弃该块 / 新预筛保留 | ✅ `old=False, new=True` |
| binary salt 块仍能命中（原有模式不回退） | ✅ |
| tasklist 内存列解析 `"135,568 K"` → `135568` KB | ✅ |

### 版本与部署

版本 0.3.14 → **0.3.15**（`Cargo.toml` / `Cargo.lock` 主包 / `.temp/build_msi.py:33`；注意 Cargo.lock 的 `errno 0.3.14` 是第三方依赖，未动）。重打 exe（md5 `36ecb0f7...`）→ 复制 assets → `cargo build --release` → 出 0.3.15 MSI。

> 仍未做真机端到端验证：本机当前无 agent 任务，key 不在内存。修复把"已知必然超时"变成"秒级命中"，最终需在换机且 Trae 刚跑过 agent 任务时确认。
