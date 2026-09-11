# CodeArts Doer 本地用量扫描方案（免鉴权）

> 适用范围：token-monitor v0.50.0
> 本文所有路径、字段名、行号均为**本机实测 + 源码核实**结果，未实测部分已显式标注为「待验证」。
> 与 `docs/codeartsdoer_接入实施规格.md` 的关系：那份解决**额度（limits）**接入，本文解决**用量（usage）**接入，两者互补而非替代。

---

## 1. 结论先行

**可以，而且比走鉴权简单得多。**

| 维度 | 走 webserver 鉴权（原方案） | 本地直扫 SQLite（本方案） |
|---|---|---|
| 用量统计（today/month/allTime） | 需 Basic 凭据，凭据位置未定位 | ✅ **免鉴权，直接读库** |
| 模型分布 / 项目归属 | 依赖接口字段 | ✅ 库中原生包含 |
| 历史趋势图 | 依赖接口 | ✅ 可由 `time.created` 自行聚合 |
| 额度 / 配额剩余（limits） | 唯一可行路径 | ❌ 做不到，必须走接口 |
| 实现复杂度 | 高（凭据 + 白名单 + 网络） | 中（复用 `qodercn` 范式，~400 行） |

**一句话**：用量走本地扫描，额度仍需鉴权，两者是**不同的数据源**，不要混为一谈。

---

## 2. 数据落点：实测结果

前置文档判断「本地无用量数据」——**这个结论是错的**。数据确实落在本地，只是路径不在预期位置。

```
❌ ~/.codeartsdoer/                     只有配置、skills、rules、agents 缓存、codebase 索引
❌ 项目/.codeartsdoer/                  只有 file-index.db（代码索引）、mcp、skills 状态
❌ IDEA plugins/CodeArts_Agent_*/       只有 lib/（jar 包）
❌ IDEA system 目录                      只有 IDEA 自身缓存 db

✅ ~/.local/share/.codeartsdoer/opencode.db      ← 真正的用量库
   ├── opencode.db        12.5 MB（2026-09-01 15:47 更新）
   ├── opencode.db-wal     4.1 MB（16:58 仍在写）
   ├── opencode.db-shm
   ├── storage/session_diff/ses_*.json  （会话 diff）
   └── checkpoint/  cron/  tool-output/
```

**关键发现**：文件名是 `opencode.db`，且 webserver exe 内嵌字符串含 `message.jsonl` / `opencode.db` / `prompt-history.jsonl`
—— 说明 CodeArts Doer 的 webserver **基于 opencode 改造**。这带来两个直接收益：

1. schema 与项目已有的 opencode 解析器**字段对齐**，`json_extract` 路径可直接照搬；
2. 项目已有 `src/shared/opencodeSession.js` 的抽取约定可复用。

---

## 3. 数据结构规格（实测）

### 3.1 表清单

`opencode.db` 共 18 张表，与用量相关的只有三张：

| 表 | 行数（实测） | 用途 |
|---|---|---|
| `message` | 745（654 条含 tokens） | **主数据源**，assistant 消息携带 token 用量 |
| `part` | 3459（640 条含 tokens） | 消息分片，亦含 tokens/cost |
| `session` | 22 | 会话元数据（`data` 列实测为 NULL） |
| `project` | 4 | 项目记录 |

> 其余：`account` / `control_account` / `event` / `event_sequence` / `todo` / `permission` / `preference` / `workspace`
> / `cag_*` / `__drizzle_migrations` —— 与用量无关，其中 `account` / `control_account` 为空表（0 行）。

### 3.2 `message.data` 的 JSON 结构（实测样本）

```json
{
  "parentID": "msg_05c2bad12001gWXXpKn7J0drPi",
  "role": "assistant",
  "mode": "build",
  "agent": "build",
  "path": { "cwd": "E:\\project\\dbpt-zw", "root": "E:\\project\\dbpt-zw" },
  "cost": 0,
  "tokens": {
    "total": 60911,
    "input": 60548,
    "output": 362,
    "reasoning": 1,
    "cache": { "write": 0, "read": 0 },
    "context": { "tools": 33778, "mcp": 0, "messages": 760, "skills": 0, "system_prompts": 26373 }
  },
  "modelID": "GLM-5.2",
  "providerID": "inferhub-provider",
  "time": { "created": 1788253078796, "completed": 1788253096015 },
  "finish": "stop"
}
```

### 3.3 字段对照：`opencodeSession.js` 的抽取路径完全适用

项目已有实现（`src/shared/opencodeSession.js:116-121`）：

```js
116:          json_extract(data,'$.cost')             AS cost,
117:          json_extract(data,'$.tokens.input')     AS tInput,
118:          json_extract(data,'$.tokens.output')    AS tOutput,
119:          json_extract(data,'$.tokens.reasoning') AS tReasoning,
120:          json_extract(data,'$.tokens.cache.read')  AS tCacheRead,
121:          json_extract(data,'$.tokens.cache.write') AS tCacheWrite
```

同一套 `json_extract` 路径在 CodeArts 库上实测可用，无需改动。

### 3.4 ⚠️ 必须遵守的 total 计算约定

`src/shared/opencodeSession.js:141-144` 的注释是硬约束：

```js
141:  // Match how tokscale totals a session: input + output + cacheRead + cacheWrite. OpenCode's
142:  // stored `tokens.total` ADDS reasoning on top of that, so trusting it over-counts vs the
143:  // session card. Keep reasoning informational only — same convention as Claude/Codex.
144:  const total = input + output + cacheRead + cacheWrite;
```

**不要**直接读 `tokens.total`（它把 reasoning 加进去了，会比其他客户端偏高）。

### 3.5 ⚠️ `cost` 恒为 0

实测所有 `message.data.cost` 均为 `0`，CodeArts Doer **不写成本字段**。
需要货币化时必须自行估算，参照 `qoderCnUsage.js:229-255` 的 `resolveQoderCnPricing()`。

---

## 4. 端到端验证（实测）

在 webserver 进程持有数据库（WAL 活跃写入）的情况下，以**只读方式**直连生产库，成功聚合：

```
DB        : C:\Users\Lenovo\.local\share\.codeartsdoer\opencode.db
rows w/ tokens: 654
--- totals (input+output+cacheRead+cacheWrite) ---
  today  : 8,249,446 (110 msgs)
  month  : 8,249,446
  allTime: 44,877,331
--- today by project ---
     8,249,446  E:\project\dbpt-zw
--- allTime by model (top 6) ---
    22,194,360  deepseek-v4-pro-0813
    12,731,225  GLM-5.2
     9,951,746  deepseek-v4-pro
             0  Pangu_Doer_in_CodeArts
```

验证方式：

```js
const { DatabaseSync } = require('node:sqlite');
const db = new DatabaseSync(dbPath, { readOnly: true });
db.exec('PRAGMA busy_timeout = 250');
```

**结论**：只读直连安全可行，`busy_timeout = 250` 足以应对 WAL 并发写入。

### 4.1 ⚠️ `sqlite3` CLI 本机不存在

```
$ sqlite3 --version
bash: sqlite3: command not found
```

而 `qoderCnUsage.js:371` 的**主路径依赖 `sqlite3` CLI**：

```js
371:    const result = await run('sqlite3', cliArgs, {
372:      encoding: 'utf8', maxBuffer: maxReadBytes, timeout: 30_000, windowsHide: true
373:    });
```

CLI 缺失时会降级到 `node:sqlite`（`qoderCnUsage.js:386-399`），功能上没问题，但每次采集都会先失败一次再降级。
**建议**：新适配器把 `node:sqlite` 作为主路径，CLI 作为可选加速路径。

### 4.2 待验证

- `Pangu_Doer_in_CodeArts` 模型 total 为 0：该模型的 `tokens` 结构可能与常规不同（未抽样确认），实现时需按模型名容错。
- macOS / Linux 上的数据路径是否同为 `~/.local/share/.codeartsdoer/`：本方案只在 Windows 实测。

---

## 5. 接入架构

项目已内置「本地解析客户端」机制 —— `PARSE_LOCAL_CLIENTS`，`proma` 与 `qodercn` 都走这条路，
**不经过 tokscale 二进制，也不需要任何鉴权**。

```
                        collectUsageOnce(clients)
                                  │
                    normalizeClientsCsv(clients)
                                  │
              ┌───────────────────┴────────────────────┐
              │  collector.js:1492                      │
              │  localClients = new Set(PARSE_LOCAL_CLIENTS)
              └───────────────────┬────────────────────┘
                    ┌─────────────┴──────────────┐
             属于 localClients              其余（tokscaleClients）
                    │                            │
        ┌───────────┴───────────┐                │
        ▼                       ▼                ▼
  promaUsage.js          qoderCnUsage.js   tokscale 二进制
  (JSONL 扫描)           (SQLite 直读)     (spawn --json)
        │                       │                │
        │        ⬇ 新增         │                │
        │      codeartsdoerUsage.js               │
        │      (SQLite 直读, 免鉴权)              │
        └───────────┬───────────┘                │
                    │  buildXxxPeriods()          │
                    │  → 伪 tokscale JSON          │
                    │  → extractUsageFromTokscale()│
                    └─────────────┬────────────────┘
                                  ▼
                     mergePeriods → today / month / allTime
```

关键点：本地适配器通过 `buildTokscaleJson()` 把自己伪装成 tokscale 输出，
再走统一的 `extractUsageFromTokscale()`，因此**下游完全无感知**。

---

## 6. 实施清单

新增 `src/shared/codeartsdoerUsage.js`，严格对照 `qoderCnUsage.js`（526 行）实现。

### 6.1 必须导出的函数（对照 `qoderCnUsage.js:515` module.exports）

| 函数 | 对照实现 | 说明 |
|---|---|---|
| `codeartsdoerDataPaths(options)` | `qoderCnUsage.js:289-307` | 定位 db 路径，支持 env 覆盖 |
| `collectCodeartsdoerRows(options)` | `:414-434` | 读行 + 按 `id` 去重 |
| `buildCodeartsdoerPeriods(options)` | `:472-484` | → `{today, month, allTime}` |
| `buildCodeartsdoerHistoryGraph(options)` | `:492-513` | 历史趋势图 |
| `resolveCodeartsdoerPricing(rows, opts)` | `:229-255` | 因 `cost=0`，必须自行估算 |

### 6.2 数据路径解析

```js
// 对照 qoderCnUsage.js:289-307，但路径不同：
// QoderCN   : <appSupport>/QoderCN/...
// CodeArts  : ~/.local/share/.codeartsdoer/opencode.db   （不随平台变化，Windows 实测）
const dbPath = path.join(os.homedir(), '.local', 'share', '.codeartsdoer', 'opencode.db');
// 建议支持 env 覆盖（照抄 :301 的模式）：
//   TOKEN_MONITOR_CODEARTSDOER_DB_PATH
```

### 6.3 SQL 主体

```sql
SELECT
  id                                      AS messageId,
  json_extract(data,'$.time.created')     AS created,
  json_extract(data,'$.modelID')          AS modelID,
  json_extract(data,'$.providerID')       AS providerID,
  json_extract(data,'$.cost')             AS cost,
  json_extract(data,'$.tokens.input')     AS tInput,
  json_extract(data,'$.tokens.output')    AS tOutput,
  json_extract(data,'$.tokens.reasoning') AS tReasoning,
  json_extract(data,'$.tokens.cache.read')  AS tCacheRead,
  json_extract(data,'$.tokens.cache.write') AS tCacheWrite,
  json_extract(data,'$.path.cwd')         AS cwd
FROM message
WHERE json_extract(data,'$.tokens') IS NOT NULL
  AND json_extract(data,'$.time.created') >= ?
```

按 `id` 去重（对照 `qoderCnUsage.js:431-433` 按 `messageId` 去重）。

### 6.4 读取预算保护（照抄）

| 机制 | 对照实现 |
|---|---|
| `maxReadBytes` / `maxReadRows` | `qoderCnUsage.js:309-317` |
| `boundedRows()` | `:330-344` |
| `readBudgetError()` / `isReadBudgetError()` | `:319-328` |
| 失败时**大声报错**而非返回空 | `:405-409`（注释明确要求） |

### 6.5 接线点（逐处，共 11 处）

| # | 文件 | 行号 | 改动 |
|---|---|---|---|
| 1 | `src/shared/clientTracking.js` | 8 | `PARSE_LOCAL_CLIENTS` 增加 `'codeartsdoer'` |
| 2 | `src/shared/clientTracking.js` | 24-28 | `KNOWN_CLIENTS` 用 `insertClientBefore` 插入（opt-in，不进 `DEFAULT_CLIENTS`） |
| 3 | `src/shared/collector.js` | 47-52 区段 | require 新增的 5 个函数 |
| 4 | `src/shared/collector.js` | 1495 附近 | 增加 `includesCodeartsdoer` 判定 |
| 5 | `src/shared/collector.js` | 1503-1507 区段 | 增加 `codeartsdoerReadState` |
| 6 | `src/shared/collector.js` | 1522-1525 区段 | 增加 periods / rows / pricing 变量 |
| 7 | `src/shared/collector.js` | 1529-1530 区段 | `emitProgress` 中 merge |
| 8 | `src/shared/collector.js` | 1571-1593 区段 | 采集主体 + 失败 fallback |
| 9 | `src/shared/collector.js` | 1641-1642 区段 | `freshPartitions` / anchor 处理 |
| 10 | `src/shared/collector.js` | 1708-1711 区段 | 最终 merge 进 today/month/allTime |
| 11 | `src/shared/collector.js` | 1896 / 1909 / 1923-1924 / 1940-1941 | 历史图表构建与回传 |

> 行号基于 v0.50.0。新增代码插在 qodercn 对应位置之后即可，改动会使其后的行号整体偏移，
> 实施时**从后往前改**可避免行号漂移困扰。

### 6.6 其他配套（沿用「新增 client 需改 11 处」的项目约定）

| 位置 | 说明 |
|---|---|
| `src/electron/renderer/app.js` | `LIMIT_PROVIDERS` / 客户端显示映射 |
| `src/electron/renderer/limitProviderPresentation.js` | 展示层文案 |
| `assets/` | 客户端图标资源 |
| 多语言 README（en / zh-CN / zh-TW / ja / ko） | 支持的客户端列表 |
| `tests/shared/verifyVendoredTokscaleClients.test.js:6,14` | `PARSE_LOCAL_CLIENTS` 校验会自动生效 |

⚠️ **client id 必须是 `normalizeClientName()` 的不动点**，否则 watch 增量扫描会把该 client 的分区清零
（详见 AGENTS.md）。

⚠️ **不要**把 `codeartsdoer` 加进 `LIMIT_PROVIDER_IDS`（`limitProviders.js:5-10`）——
实测 `qodercn` 也不在其中，本地适配器只做用量、不做额度。

---

## 7. 风险与边界

| 风险 | 等级 | 说明与对策 |
|---|---|---|
| schema 变更 | **高** | 项目注释已明示此类本地适配器的固有风险（`clientTracking.js:22-23`：*"a local adapter that may break when Qoder changes its DB schema"*）。对策：schema probe（照抄 `qoderCnUsage.js:125` 的表探测 + 负缓存），失败时报错并保留上次快照。 |
| WAL 并发 | 中 | 实测 `readOnly + busy_timeout=250` 可行；仍建议保留 CLI 降级路径。 |
| `cost` 恒为 0 | 中 | 必须走 pricing 估算，与 `qodercn` 一致。 |
| 数据路径跨平台 | 中 | 仅 Windows 实测，需在 macOS/Linux 上确认 `~/.local/share/.codeartsdoer/`。 |
| `Pangu_Doer_in_CodeArts` total=0 | 低 | 该模型 tokens 结构可能不同，按模型名容错。 |
| 重复计数 | 低 | 若 tokscale 未来原生支持该路径，需从 `PARSE_LOCAL_CLIENTS` 移除，否则双计。 |

---

## 8. 变更清单

| 类型 | 文件 | 说明 |
|---|---|---|
| 新增 | `src/shared/codeartsdoerUsage.js` | 本地 SQLite 用量适配器（~400 行，对照 `qoderCnUsage.js`） |
| 修改 | `src/shared/clientTracking.js` | `PARSE_LOCAL_CLIENTS` + `KNOWN_CLIENTS` |
| 修改 | `src/shared/collector.js` | 11 处接线（见 6.5） |
| 修改 | `src/electron/renderer/app.js` | 客户端显示映射 |
| 修改 | `src/electron/renderer/limitProviderPresentation.js` | 展示层文案 |
| 新增 | `assets/` | 客户端图标 |
| 修改 | `README.md` + 4 个多语言版本 | 支持客户端列表 |
| 新增 | `tests/shared/codeartsdoerUsage.test.js` | 单元测试（用 fixture db） |
| 新增 | `tests/shared/` guard 测试 | schema 漂移防护 |

> `worker/` 侧无需改动：本地适配器依赖 Node `node:sqlite` / `sqlite3` CLI，
> Worker 不采集用量，且 `npm run sync:worker` 只同步共享闭包——新增文件需确认是否纳入同步清单。

---

## 9. 与额度方案的关系

```
CodeArts Doer
     │
     ├── 用量（usage）── 本地 SQLite ──→ 本方案 ✅ 免鉴权
     │                    ~/.local/share/.codeartsdoer/opencode.db
     │
     └── 额度（limits）── 云端 / webserver API ──→ 仍需 Basic 鉴权
                          127.0.0.1:33586 (WWW-Authenticate: Basic)
                          见 docs/codeartsdoer_接入实施规格.md
```

两者可**独立推进**：本方案不依赖凭据定位结果，建议优先落地。
