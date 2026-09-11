# Gemini CLI 本地 Token 计算模式分析 与 接入统计工具流程

> 背景：当前 `atomcode_local_stats.py` 只扫描 `~/.atomcode/sessions`，**完全没有 Gemini 的解析路径**，所以本地用 Gemini CLI 产生的用量从未被统计。本文档分析 Gemini 的 token 计算/存储模式，并给出把它接入统计工具的大致流程。
>
> 分析环境：本机 `~/.gemini/tmp/**/chats/*.jsonl`，共 121 个会话文件，覆盖 2026-04-24 ~ 2026-06-30。

---

## 1. 为什么 Gemini 没被统计上

不是 token 计算 bug，而是**根本没有 Gemini 解析器**：

| | 数据目录 | 格式 | 是否被当前脚本解析 |
|---|---|---|---|
| AtomCode | `~/.atomcode/sessions/**/*.{meta,jsonl}` | 整体 JSON + JSONL | ✅ 已支持 |
| Gemini | `~/.gemini/tmp/<project>/chats/**/*.jsonl` | 纯 JSONL 追加日志 | ❌ 无解析路径 |

两者的目录、文件命名、字段结构完全不同。脚本只认 AtomCode 的 `*.meta`/`*.jsonl`，Gemini 文件被完全忽略。

---

## 2. Gemini 的 token 存储模式（核心分析）

### 2.1 文件布局
```
~/.gemini/tmp/
├── dbpt/           ← 项目 A（主）
│   ├── chats/
│   │   ├── session-2026-06-04T13-25-4e2479a8.jsonl   ← 旧式命名
│   │   └── b8a1cddf-.../9ef40476-....jsonl            ← 新式命名（uuid/uuid）
│   └── tool-outputs/   ← 工具返回体（与 token 无关，忽略）
├── dbpt-dw/        ← 项目 B
└── dbpt-dw-demo/   ← 项目 C（注意是 .json 形态）
```
两种文件命名同构，token 行结构一致（见 2.3）。

### 2.2 行类型（单文件内混合三种）
1. **会话头**（每文件 1 行）：`{sessionId, projectHash, startTime, lastUpdated, kind}` —— 无 token。
2. **token 记录行**：`{id, timestamp, type:"gemini", content, thoughts, tokens, model}` —— **token 只在这里**。
3. **`$set` 更新行**：`{$set: {messages:[...], lastUpdated}}` —— 只补 messages，**不带 token**，可忽略。

> 只有 `type == "gemini"`（助手轮）带 token；用户轮、工具轮都不带。

### 2.3 token 字段
```json
"tokens": { "input": 10894, "output": 193, "cached": 0, "thoughts": 253, "tool": 0, "total": 11340 }
```

### 2.4 关键结论（已程序化验证）

**结论 A —— token 是「逐轮」的，不是累计的。**
相邻轮次的 `input` 序列上下跳动（10894→11615→12093→52366→…后又掉回 17 万），若累计只会单调递增。所以每个 `type` 行的 6 个字段是该**单轮请求**的量，可直接跨轮求和。

**结论 B —— `cached` 是 `input` 的子集，绝不能与 `input` 相加。**
- 全量 127,207 个 token 行中，`cached > input` 出现 **0 次**。
- `total == input + output + thoughts + tool` 在 **全部 127,207 行**成立；而 `total == input+output+cached+thoughts+tool` 只在 3,245 行成立（恰好 cached=0）。
- ⇒ 计费口径：**`total` 已把 thoughts 单独计入、并刻意排除 cached 以避免重复**。统计时若写成 `input+cached` 会翻倍。

映射到本工具现有口径：`input`→输入、`output`→输出、`cached`→缓存读取（⊂输入）、`thoughts`→推理/reasoning。

**结论 C —— 海量重复行（最大陷阱，不去重会多算约 6.8 倍）。**
- 全量 127,207 条 token 行，去重后仅 **18,769 个唯一 `id`（轮次）**。
- 同一 `id` 完全同值重复 **108,438 次**，平均每个轮次被记 **~6.8 次**（疑似会话每次加载都把完整历史重新追加一遍）。
- 同 `id` **不同** token 值冲突 = **0 次** → 去重安全：按 `id` 取任意一条（或取 `total` 最大）即可。
- 若不去重直接求和，总量会膨胀约 **6.8 倍**。

**结论 D —— 无会话级汇总。**
会话头只有 `sessionId/startTime/kind`，没有 token 合计。必须**逐行解析 + 按 `id` 去重**才能得到正确数字。

---

## 3. 真实数据规模（去重后，本机实测）

| 维度 | 数值 |
|---|---|
| 唯一轮次（id） | 18,769 |
| 时间跨度 | 2026-04-24 ~ 2026-06-30 |
| 合计 token（Σtotal） | **2,909.7M** |
| ├ 输入 input | 2,901.2M（含缓存部分） |
| ├ 输出 output | 5.06M |
| ├ 缓存读取 cached（⊂input） | 2,456.1M → 约占 input 的 **85%** |
| └ 推理 thoughts | 3.49M |

**按模型**（轮次 / 合计 token）：
| 模型 | 轮次 | 合计 |
|---|---|---|
| gemini-3-flash-preview | 16,826 | 2,564.1M |
| gemini-3.1-pro-preview | 1,345 | 259.8M |
| gemini-3.1-flash-lite | 402 | 41.1M |
| gemini-3-pro-preview | 148 | 40.3M |
| gemini-3.5-flash | 37 | 1.3M |

> 对比 AtomCode 侧合计 ~1.07B token，Gemini 本地用量约为其 **2.7 倍**，且缓存命中率同样很高（与 AtomCode 一致）。

---

## 4. 接入统计工具的大致流程（路线图）

把 Gemini 并入 `atomcode_local_stats.py` 的增量步骤：

1. **扫描目录**：`glob(~/.gemini/tmp/**/chats/**/*.jsonl)`，覆盖 `dbpt`/`dbpt-dw`/`dbpt-dw-demo`（注意 `dbpt-dw-demo` 是 `.json`，需单独处理或暂略）。
2. **逐行解析**：跳过非 dict、跳过 `$set` 行；只取 `isinstance(o.get("tokens"), dict)` 且 `o.get("type")=="gemini"` 的行。
3. **按 `id` 去重**：用 `dict[id] = 取 total 最大的那条`，消除 ~6.8 倍重复。
4. **字段映射**：`input→input`、`output→output`、`cached→cached_input`、`thoughts→reasoning`、`model→模型名`。
5. **聚合**：复用现有 `aggregate()` 框架，按模型 / 项目 / 天累加（项目名可取 chats 的父目录 `dbpt`/`dbpt-dw`）。
6. **归并总览**：`--all` 文本、CSV、JSON、HTML 看板自动包含 Gemini 数据；与 AtomCode 并列展示。

### 必须避开的坑
- ❌ `input + cached` 相加 → 重复计数（cached ⊂ input）。
- ❌ 不去重直接求和 → 多算 ~6.8 倍。
- ❌ 把 `$set` 行当 token 行 → 取不到 tokens 字段。
- ✅ 缓存读取占比很高，可视化时与 AtomCode 口径一致处理即可。

### 性能提示
单会话文件可达 38 万行（如 dbpt 某 session 文件），dbpt-dw 有 62 个文件、5.4 万条 token 行。逐行流式读取 + 去重 dict 即可，本机实测秒级完成，无内存压力。

---

## 5. 已实现：`gemini_local_stats.py`

按本文档的 4 条结论实现了独立统计器（纯本地 · 无联网 · 无成本，仅标准库）。

```bash
python gemini_local_stats.py --all              # 全部维度
python gemini_local_stats.py --days 7           # 最近 7 天
python gemini_local_stats.py --since 2026-06-01
python gemini_local_stats.py --debug            # 打印解析/去重自检
python gemini_local_stats.py --csv out.csv --json out.json --html out.html
```

对外可复用入口：`from gemini_local_stats import build` → `build(since_ms, root)` 返回 `(聚合结果, 解析统计)`，
可直接并入 `atomcode_local_stats.py` 做统一看板。

### 5.1 实现要点（对应上文结论）
| 结论 | 代码中的落点 |
|---|---|
| A 逐轮可求和 | 每条 `type` 行的 6 个字段直接累加 |
| B `cached ⊂ input`，禁相加 | 合计一律取 `Σ total`，`cached` 只在"缓存读取"列单独展示 |
| C 必须按 id 去重 | `load_gemini()` 全局 `dict[id]`，同 id 取 `total` 最大者 |
| D 无会话级汇总 | 会话头/`$set` 行直接跳过，逐行解析后按文件归并出会话 |

性能：`.jsonl` 流式逐行读，先用 `'"tokens"' in line` 做廉价预筛再 `json.loads`，
避免对海量 `$set`/`user` 行做无谓解析。本机 190 个文件 / 109 万行，扫描约 **65 秒**。

### 5.2 对本文档的重要补充：`.json` 文件同样带 token

第 4 节把 `*.json`（dbpt-dw-demo / dbpt-zw-demo / element-demo / desktop）标为"需单独处理或暂略"。
实测这些文件是**整文件美化 JSON**，结构为 `{sessionId, startTime, messages:[...]}`，
`messages[]` 内同样含 `type=="gemini"` + `tokens`，且**与 .jsonl 的会话 id 零重叠**。
因此已一并纳入统计，数据量比第 3 节多出一截：

| 口径 | 唯一轮次 | 合计 token | 输入 | 输出 | 缓存读取 | 推理 |
|---|---|---|---|---|---|---|
| 仅 .jsonl（第 3 节） | 18,769 | 2,909.7M | 2,901.2M | 5.06M | 2,456.1M | 3.49M |
| **全量（.jsonl + .json）** | **27,337** | **4.10B** | 4.09B | 7.82M | 3.44B | 4.64M |

全量口径下的自检结果：同 id 值冲突 **0**、`cached > input` **0**、`total` 等式不成立 **0**、
缺时间戳 **0**、缺 model **11** 行（归入"(未记录模型)"）。缓存命中率 84.2%。

时间跨度也从 2026-04-24 ~ 06-30 扩展到 **2026-04-03 ~ 06-30**。

### 5.3 仍未做的事（待确认）
- 与 AtomCode 口径合并：两侧"合计"定义不同（AtomCode 目前是 `input+output+cached`，
  Gemini 是 `input+output+thoughts+tool` 且 cached 不重复计入），合并前需先统一口径；
- 将 `.gemini/tmp` 的目录名（dbpt / dbpt-dw / …）映射为本机真实工程路径；
- 打包 exe、纳入每日 22:00 自动化。
