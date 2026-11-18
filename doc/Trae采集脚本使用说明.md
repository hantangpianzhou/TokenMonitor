# Trae 采集脚本使用说明

> 位置：`scripts/trae_collector.py` · 2026-11-17
> 作用：把本机 Trae（`chat_turn.context.token_usage`）的 token 用量采集进 TokenMonitor 的 `usage_records`（`provider='trae'`）。

---

## 一、它做什么（三步走）

```
缓存 key 存在? ──否──> 扫 Trae 进程内存提 key ──失败──> 提示「请先发起一次 agent 任务再重试」
     │是                        │
     ▼                          ▼
HMAC 校验通过? ──否──┘      保存 key 到缓存
     │是
     ▼
复用缓存（跳过扫描，~1s）─> 解密主库 ─> 解析 chat_turn ─> fingerprint 去重入库
```

| 环节 | 说明 |
|---|---|
| **key 缓存** | `%APPDATA%\TokenMonitor\trae_key.json`，含 `key_hex` / `salt_hex` / `db_size` / `verified_at` |
| **HMAC 校验** | 每次启动用**当前主库 page1 的 salt** 重算 HMAC-SHA512 校验缓存 key；Trae 重启换了 key 会校验失败，自动重提 |
| **提 key** | 扫 Trae CN / TeleAgent / ais 进程内存，用数据库 salt 做二进制锚点定位候选 key，HMAC 命中即确认；上限 60s |
| **解密** | 只读主库 `database.db`，逐页 AES-256-CBC 解密到临时明文库 |
| **去重入库** | fingerprint = `localdb:{session_id}:{created_at}:{id}`；表已有 UNIQUE 约束 + `INSERT OR IGNORE`，重复跑不产生重复行 |

---

## 二、五种运行方式

```bat
python scripts\trae_collector.py                  :: 采集（缓存 key 优先）
python scripts\trae_collector.py --check          :: 只看缓存 key 是否有效（0=有效 / 2=需重提）
python scripts\trae_collector.py --force-extract  :: 忽略缓存，强制重新从内存提 key
python scripts\trae_collector.py --dry-run        :: 解密+解析但不写库，预览条数与 token 合计
python scripts\trae_collector.py --auto           :: 【主程序自动采集用】只用缓存 key，失效静默跳过
```

依赖：`pycryptodome`（`python -m pip install pycryptodome`）。本机验证环境 Python 3.13.12 + Crypto 4.5.0。

### `--auto` 与主程序自动采集（2026-11-17 新增）

Trae 采集已集成进 TokenMonitor 主程序：**每轮完整扫描（5 分钟定时 / 手动刷新 / 启动）完成后**，主程序拉起一次内嵌的 `TraeCollector.exe --auto`，自动把 Trae 用量写进库，无需手动跑脚本。

| 项 | 说明 |
|---|---|
| **触发** | app 层 `all_done`（一轮扫描最后一个 provider）时拉起，与数显推送同一时机 |
| **模式** | `--auto` **只走缓存 key**（HMAC 校验当前库），命中即解密+幂等写库；**绝不扫内存** |
| **失效处理** | 缓存 key 失效（Trae 重启换 key）时**静默跳过本轮**，不扫内存、不提示、不拖累扫描。用户下次发起 agent 任务后手动跑一次 `--force-extract` 刷新缓存即可恢复 |
| **即时刷新** | 写入完成后通过 `async_channel` 通知 UI 重新聚合，数据无需等下一个 5 分钟周期 |
| **单文件分发** | exe 经 `include_bytes!` 编进主程序，首次释放到 `%TEMP%/tokenmonitor-trae/`；目标机无需 Python |
| **防重入** | `AtomicBool` 保证同时只有一个采集进程；退出后清标志 |
| **打包** | `python scripts\build_trae_exe.py --clean` → 复制 `scripts/dist/TraeCollector.exe` 到 `assets/TokenCollector.exe` → `cargo build --release` |

> **重要**：自动采集依赖缓存 key 已建立。首次使用需手动跑一次 `python scripts\trae_collector.py`（缓存命中则直接写，未命中则扫内存提 key），之后主程序即可自动跟随。缓存失效后同理需手动刷新一次。

---

## 三、为什么只读主库、不手工合并 WAL

实测结论（Trae 活跃、WAL 约 4.6MB 未合并时）：

| 方案 | 结果 |
|---|---|
| 纯主库快照 | 连续 3 次 `integrity_check=ok`，`chat_turn` 稳定 201 行，解密可靠 ✅ |
| 手工把 WAL 帧套回主库 | `integrity_check` 大量 btree 错误、`file is not a database` ❌ |

根因：活跃写入下，**db 快照（14552 页）与 WAL 末帧记录的 dbsz（14268）不在同一时间点**，且 WAL 中 page 1 帧带 16 字节 salt 头，手工套帧会造成页版本错乱。Trae 自身会 checkpoint 把 WAL 数据并入主库——实测主库 turn 数从上次导入的 187 涨到 201 即为证。因此只读主库即可拿到**稳定且最新**的表，无需碰 WAL。

---

## 四、提 key 失败怎么办

出现下面提示，说明当前 Trae 进程内存里没有 key：

```
✗ 提 key 失败：请先发起一次 agent 任务再重试
```

原因：SQLCipher key 只在 agent 任务运行时驻留 Trae 进程内存。处理：

1. 在 Trae 里**发起一次 agent 任务**（随便问一句，让它开始跑）；
2. 任务启动后立刻重跑 `python scripts\trae_collector.py --force-extract`；
3. 若 Trae 刚启动或长期空闲，key 可能已被换出，同样按上述重试。

> 缓存 key 一旦拿到就长期有效（只要 Trae 没重启换库 key），日常采集走快路径，不需要重复提 key。

---

## 五、本机验证记录（2026-11-17）

| 验证项 | 结果 |
|---|---|
| `--check` 缓存 key HMAC 校验 | 通过 |
| 首次正式入库 | inserted=3，trae 行 187 → 190 |
| 重复运行幂等 | inserted=0，trae 行 190 → 190 ✅ |
| 提取算法单元验证（构造 salt+key 内存块） | 命中 ✅ |
| 无 agent 任务时提 key | 60s 后正确显示失败提示 ✅ |
| `py_compile` 语法检查 | 通过 |
| `--auto --dry-run` 静默模式 | EXIT=0，无输出 ✅ |
| `--auto` 真实写库 | trae 行 196 → 203（采到 7 条新数据）✅ |
| `--auto` 重复运行幂等 | 203 → 203 ✅ |
| 打包 `TraeCollector.exe` 独立跑 `--auto` | 11.3 MB，零 Python 环境 EXIT=0 ✅ |
| 主程序内嵌验证 | release exe 46.62 MB（含 TokenScreen + TokenCollector），SHA 与 MSI 解出一致 ✅ |

> 备注：`--auto` 写库后本机 trae 行由 196 增至 203。采集后主程序通过完成通道自动刷新视图，无需手动刷新/重启。
