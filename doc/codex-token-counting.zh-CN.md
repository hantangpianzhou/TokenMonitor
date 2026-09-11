# Codex Token 统计逻辑分析

> 适用范围：Token Monitor v0.50.0（`src/shared/collector.js`、`src/shared/usage.js`、`src/shared/sessionDetail.js`）
> 分析对象：`codex` 这一个 client 的 **token 数量**是怎么被算出来的（不含额度/限速窗口，见 §7）

---

## 1. 结论速览

Codex 的 token 数量在本项目里有 **两条互相独立的统计链路**，二者口径不同但刻意保持一致：

| 链路 | 用途 | 谁在解析 jsonl | token 来源 | 入口 |
| --- | --- | --- | --- | --- |
| **A. 全局用量**<br>（today / month / allTime、按 client·model·project·session 拆分） | 托盘、仪表盘、历史曲线 | **外部二进制 `tokscale`** | tokscale 输出的每一行 usage row | `collectUsageOnce()`<br>`collector.js:1435` |
| **B. 会话明细**<br>（点进单个 session 看每轮提问的 token） | Session Detail 弹窗 | **本项目自己解析** | `event_msg / token_count` 事件的 `last_token_usage` | `parseCodexTranscript()`<br>`sessionDetail.js:133` |

一句话概括两条链路的共同口径：

```
Codex 遵循 OpenAI 约定：input_tokens 含 cached_input_tokens，output_tokens 含 reasoning_output_tokens
→ 必须做"不相交化"后再相加，否则重复计数
   total = (input - cached) + output + cached + 0(cacheWrite)
         = input + output            ← 恒等于 Codex 自己的 total_tokens
```

---

## 2. 数据源：Codex 把 token 记在哪

Codex 本机落盘的是 **JSONL 会话日志**（一行一个事件，append-only）。

| 位置 | 说明 | 代码引用 |
| --- | --- | --- |
| `$CODEX_HOME/sessions/YYYY/MM/DD/rollout-*.jsonl` | 主目录，默认 `~/.codex/sessions` | `collector.js:2200-2203` |
| `$CODEX_HOME/archived_sessions/` | 归档目录，同样被扫描 | `collector.js:2204` |
| tokscale headless 捕获目录 | `codex exec --json` 的输出捕获，非 macOS 专属，两个默认路径都要扫 | `collector.js:2101-2113`、`:2205` |

会话 id 形如 `rollout-2026-08-31T13-42-07-<uuid>`，**内嵌了本地时间戳**，这是项目推断 `startedAt` 的依据（`collector.js:794-802`）：

```js
// collector.js:798  —— 注意分隔符同时兼容 ':' 和 '-'
const localMatch = raw.match(/(\d{4})-(\d{2})-(\d{2})T(\d{2})[:-](\d{2})(?:[:-](\d{2}))?/);
```

文件路径解析（`sessionFiles.js:31-38`）走"先按 id 拼路径，拼不中再全目录递归"的两级策略：

```js
function codexSessionFile(home, sessionId) {
  const match = String(sessionId || '').match(/^rollout-(\d{4})-(\d{2})-(\d{2})T/);
  if (!match) return '';
  const filePath = path.join(home, '.codex', 'sessions', match[1], match[2], match[3], `${sessionId}.jsonl`);
  ...
}
```

---

## 3. 链路 A：全局用量统计（tokscale 扫描）

### 3.1 流程图

```
┌──────────────────────────────────────────────────────────────────────┐
│ collectUsageOnce()                            collector.js:1435      │
└───────────────────────────┬──────────────────────────────────────────┘
                            │
        ┌───────────────────┴────────────────────┐
        │                                        │
   完整扫描（冷启动/手动刷新）              锚定增量（watch 触发）
   collector.js:1667-1685                 collector.js:1597-1666
        │                                        │
        │  串行 spawn 3 次（刻意串行：并发会把        │  只 spawn 1 次 --today
        │  tokscale 打到 500% CPU，issue #15）     │  再用 applyPeriodDelta
        │                                        │  精确推出 month / allTime
        ▼                                        ▼
   tokscale --json \                       tokscale --json \
     --client codex \                        --client codex \
     --group-by client,session,model \        --group-by client,session,model \
     --today | --month | --since <date>       --today
   collector.js:417                                  │
        │                                            │
        └───────────────┬────────────────────────────┘
                        ▼
        extractUsageBundleFromTokscale()          usage.js:782
        extractUsageFromTokscale()                usage.js:804
                        │
                        ▼
        addUsageRowToPeriod(row)                  usage.js:716
        ├─ tokenValueForClient(row, 'codex')      usage.js:90
        ├─ outputValueForClient(row, 'codex')     usage.js:100
        ├─ sessionFromRow(row) → period.sessions  usage.js:494
        └─ 累加 clients[] / models[] / clientModels[][]
                        │
                        ▼
        applySessionTimestamps()                  collector.js:990-998
        补 startedAt / lastUsedAt / project 归属
```

### 3.2 命令与分组

```js
// collector.js:417
const runArgs = (filter) => ['--json', '--client', filter, '--group-by', 'client,session,model', ...flags];
```

- 周期由 tokscale 侧的 flag 决定：`--today` / `--month` / `--since <allTimeSince>`（`collector.js:1670、1677、1682`）
- `--client` 接受 CSV；Codex **没有子源别名**（`TOKSCALE_CLIENT_ALIASES` 只列了 `antigravity` 和 `pi`，`collector.js:315-318`），所以传的就是裸 `codex`
- 若 tokscale 版本不认识某个 `--client` 值，会以 exit 2 拒绝并让整次扫描失败；项目用一次 `--help` 探测做降级重试（`collector.js:380-398`）

### 3.3 字段归一化：一行 row 怎么变成 token 数

tokscale 的 JSON 字段在不同版本间有 snake_case / camelCase 两套写法，项目用"按优先级取第一个非零值"来兼容（`usage.js:14-36`）：

| 语义 | 接受的键（按顺序，取首个非零） | 行号 |
| --- | --- | --- |
| 总量 | `totalTokens, total_tokens, totalTokenCount, total_token_count, tokens, tokenCount, token_count` | `usage.js:14` |
| 输入 | `input, inputTokens, input_tokens, promptTokens, prompt_tokens, totalInput` | `usage.js:32` |
| 输出 | `output, outputTokens, output_tokens, completionTokens, completion_tokens, totalOutput` | `usage.js:33` |
| 缓存读 | `cacheRead, cacheReadTokens, cache_read_tokens, cachedTokens, cached_tokens, cacheReadInputTokens, totalCacheRead` | `usage.js:34` |
| 缓存写 | `cacheWrite, cacheWriteTokens, cache_write_tokens, cacheCreationInputTokens, totalCacheWrite` | `usage.js:35` |
| 推理 | `reasoning, reasoningTokens, reasoning_tokens` | `usage.js:36` |

取值规则（`usage.js:76-105`）：

```js
// 76-84：有 total 字段就直接用，否则回退到各分量求和
function tokenValue(obj) {
  const direct = firstNumber(obj, TOKEN_KEYS);      // 命中 total_tokens 就返回
  if (direct !== 0) return direct;
  let sum = 0;
  for (const key of TOKEN_COMPONENT_KEYS) { ... }   // 注意：分量列表里刻意不含 reasoning
  return sum;
}

// 90-95：Codex 属于"推理与输出不相交"的客户端 —— 只有没有 total 时才补 reasoning
function tokenValueForClient(obj, client) {
  const base = tokenValue(obj);
  if (!hasDisjointReasoning(client)) return base;
  const direct = firstNumber(obj, TOKEN_KEYS);
  return direct !== 0 ? base : base + Math.max(0, firstNumber(obj, REASONING_TOKEN_KEYS));
}
```

**Codex 在 disjoint-reasoning 白名单里**：

```js
// history.js:41
const TOKSCALE_DISJOINT_REASONING_CLIENTS = new Set([REASONIX_CLIENT, 'codex', 'dsh']);
```

含义：tokscale 为 codex 输出的是一个 **output 与 reasoning 互不重叠**的 JSON 契约，所以对外展示时要把 reasoning 折回 output 族（`usage.js:100-105`），保证 `cacheRead + cacheWrite + output(含reasoning)` 能对上 `totalTokens`。

### 3.4 累加维度

一行 row 同时喂给多层累加器（`usage.js:716-764`）：

| 累加目标 | 条件 | 行号 |
| --- | --- | --- |
| `period.totalTokens` | 无条件（`Math.max(0, round(tokens))`） | `:732` |
| `period.clients['codex']` | `tokens > 0` | `:740-741` |
| `period.models[model]` / `clientModels['codex'][model]` | `tokens > 0` 且能识别模型 | `:747-757` |
| `period.outputTokens` / `clientOutputs['codex']` | output 折入了 reasoning | `:736`、`:744` |
| `period.sessions[key]` | 能识别出 sessionId | `:762-763` |
| `timedTokens / timedOutputTokens / timedDurationMs` | 只有带 `performance` 的 row 才计入吞吐分子分母 | `:723-729` |

> 吞吐口径刻意"门控而非缩放"：某 row 只有在贡献了 duration 时才贡献 output，这样分子分母描述的是同一批条目，且保持为可跨设备相加的整数计数器（`usage.js:128-147` 有完整注释）。

---

## 4. 链路 B：会话明细统计（自研解析）

### 4.1 事件类型识别

`parseCodexTranscript()` 逐行扫 jsonl（`sessionDetail.js:133-177`），只认 4 类行：

| 行特征 | 作用 | 行号 |
| --- | --- | --- |
| `type:'response_item'` 且 payload 是 `function_call` / `custom_tool_call` / `tool_search_call` | 记为待挂靠的工具名 | `:142-144` |
| `type:'event_msg'` 且 `payload.type === 'mcp_tool_call_end'` | 同上（MCP 工具） | `:145-147` |
| `type:'event_msg'` 且 `payload.type === 'user_message'` | **一轮对话的分界**（prompt） | `:148-155` |
| `type:'event_msg'` 且 `payload.type === 'token_count'` | **一轮回复的 token**（turn） | `:156-174` |

### 4.2 token 计算（核心）

```js
// sessionDetail.js:156-174
} else if (obj.type === 'event_msg' && payload.type === 'token_count') {
  const u = payload.info && payload.info.last_token_usage;
  if (!u) continue;                                    // 会话开始/空闲心跳，没有本轮用量 → 丢弃
  const cacheRead = num(u.cached_input_tokens);
  const tokens = makeTokens({
    input: Math.max(0, num(u.input_tokens) - cacheRead), // ① 输入减去缓存，做成与缓存不相交
    output: u.output_tokens,                             // ② 输出保持原样（已含 reasoning）
    cacheRead,
    cacheWrite: 0,                                       // ③ Codex 不报 cache_write
    reasoning: u.reasoning_output_tokens                 // ④ 仅作展示，不计入 total
  });
  if (tokens.total === 0) { pendingTools = []; continue; } // 全零的记账心跳 → 丢弃
  events.push({ kind: 'turn', timestamp: obj.timestamp || '', tokens, tools: uniqueTools(pendingTools) });
  pendingTools = [];
}
```

```js
// sessionDetail.js:13-20
function makeTokens({ input = 0, output = 0, cacheRead = 0, cacheWrite = 0, reasoning = 0 }) {
  const total = num(input) + num(output) + num(cacheRead) + num(cacheWrite);
  return { input, output, cacheRead, cacheWrite, reasoning, total };
}
```

**三条铁律**：

1. **`last_token_usage` 是"本轮增量"，不是累计值**。同一个会话里连续两条 `token_count` 分别报 5000/6000，是两轮各自的用量，逐条累加即为会话总量（测试佐证：`tests/shared/sessionDetail.test.js:79-104`，断言两个 turn 的 total 分别是 5200 和 6300，而非 6300 一个）。`total_token_usage` 字段被刻意忽略。
2. **input 必须先减 cached**，否则 `input + output + cacheRead` 会大于 Codex 自己的 `total_tokens`。
3. **reasoning 是 output 的子集**，绝不能再加一遍（代码注释里明确写了"the original bug"就是这么来的，`sessionDetail.js:159-162`）。

### 4.3 数值示例（取自仓库测试的真实断言）

输入 `last_token_usage`：

```
input_tokens           = 5000
cached_input_tokens    = 4000
output_tokens          =  200
reasoning_output_tokens=   50
total_tokens           = 5200     ← Codex 自己报的：input + output
```

项目产出：

```
input      = 5000 - 4000 = 1000   ← 与缓存不相交
cacheRead  = 4000
output     = 200                  ← 保持完整（已含 reasoning=50）
cacheWrite = 0                    ← Codex 不提供
reasoning  = 50                   ← 仅展示
total      = 1000 + 200 + 4000 + 0 = 5200  ✅ 与 total_tokens 一致
```

❌ 错误算法（若不做不相交化）：`5000 + 200 + 4000 + 50 = 9250`，虚高 78%。

### 4.4 为什么丢弃"空 tick"

Codex 会在会话开始、空闲心跳时刷 `token_count`，这两类必须丢掉，否则会话里会凭空多出若干 0 token 的"回复"：

- `info.last_token_usage` 为 `null` → 丢弃（`sessionDetail.js:158`）
- 五个字段全为 0 → 丢弃，并且**连带清空已攒的工具名**（`sessionDetail.js:171`），避免工具被挂到下一轮

测试：`tests/shared/sessionDetail.test.js:106-116`。

### 4.5 归类与汇总

```
events[]（prompt / turn 交织）
    │
    ├─ 遇到 prompt → 结束上一个 exchange，开一个新的       sessionDetail.js:217-221
    ├─ 遇到 turn   → 追加到当前 exchange.turns，addTokens 累加，合并工具名
    └─ 收尾 finalizeExchange()                          sessionDetail.js:204-212
         ├─ turnCount = turns 中 type !== 'compaction-summary' 的数量（后台压缩不计数）
         ├─ tools = 所有 turn 工具名去重
         └─ tokensAvailable = 所有 turn 都带 token 数据
    │
    ▼
filterExchangesByPeriod(today|month|total)              sessionDetail.js:256-270
    │  按 turn 粒度过滤，重算每个 exchange 的 token 与时间边界
    ▼
distributeCost(exchanges, sessionCost)                  sessionDetail.js:272-282
      按 token 占比把 tokscale 给的整会话成本摊到每轮（Codex 本身不报成本）
```

---

## 5. 两种口径对照表

| 维度 | 全局用量（链路 A） | 会话明细（链路 B） |
| --- | --- | --- |
| 解析方 | tokscale（Rust 二进制，外部） | 本项目 JS |
| 数据源 | 扫描目录 → 按 client/session/model 聚合 | 单个 jsonl 全文 |
| 输入是否减 cached | **否**（tokscale 已给出不相交的 row） | **是**（`input - cached`，`sessionDetail.js:165`） |
| reasoning 处理 | 无 total 时才补进 total；展示时折进 output | 永不进 total，仅作展示字段 |
| cacheWrite | 有则计 | 恒为 0（Codex 不报） |
| 成本 | tokscale 直接给出 | 按 token 占比从会话总成本分摊 |
| 时间归属 | tokscale 按 flag 过滤（`--today` 等） | 按 turn 时间戳 `withinPeriod()` 过滤 |
| 刷新时机 | 每次 tick / watch 增量 | 打开弹窗时才读（`sessionDetailResolver.js`） |

---

## 6. 会话时间与项目归属

tokscale 只给 token，不给 `startedAt/lastUsedAt/项目`，这部分由 `applySessionTimestamps()` 回补（`collector.js:990-998`）：

```js
const codexIds = byClient.get('codex') || new Set();
const missingCodexIds = new Set();
for (const sessionId of codexIds) {
  const filePath = codexSessionFile(home, sessionId);   // 先按 id 拼路径
  if (filePath) applyFile('codex', sessionId, filePath);
  else missingCodexIds.add(sessionId);                  // 拼不中 → 交给递归兜底
}
const codexFiles = findSessionFiles(path.join(home, '.codex', 'sessions'), missingCodexIds);
for (const [sessionId, filePath] of codexFiles) applyFile('codex', sessionId, filePath);
```

```js
// collector.js:953-960
const applyFile = (client, sessionId, filePath) => {
  const startedAt  = timestampFromSessionId(sessionId);        // rollout-2026-08-31T13-42-07 → ISO
  const lastUsedAt = lastJsonlTimestamp(filePath) || startedAt;// 读文件尾 64KB 的最后一个时间戳
  const identity   = resolveProjects ? projectIdentity(projectPathFromJsonl(filePath)) : {};
  ...
};
```

注意 `collector.js:1108`：

```js
if (!['claude', 'codex', 'opencode', 'dsh'].includes(ref.client)) resolvedSessionKeys.add(key);
```

Codex 被**排除在"已解析完成"集合之外** —— 也就是说每个 tick 都会重新尝试解析 Codex 会话的项目归属，因为 Codex 的 jsonl（相对 Claude）更容易在写入中途被读到不完整的项目路径。

---

## 7. 与"额度 / 限速"的区别（易混淆）

`src/shared/limitCollector.js` 里也有 190+ 处 `codex`，但那统计的是 **ChatGPT 账号的 rate-limit 窗口剩余百分比**（5 小时 / 周额度、重置时间），**不是**已消耗的 token 数。

| | 本文的 token 数 | limitCollector 的额度 |
| --- | --- | --- |
| 数据来源 | 本机 jsonl 文件 | ChatGPT 后端 API（`wham/usage` 等） |
| 语义 | 已实际消耗的 token | 剩余配额百分比 + 重置时刻 |
| 是否需要登录 | 否 | 是（`codexAuth.js` / `codexLogin.js`） |
| 相关模块 | `usage.js` / `sessionDetail.js` / `collector.js` | `limitCollector.js` / `codexResetForecast.js` |

---

## 8. 易错点清单

| # | 坑 | 现状处理 | 代码位置 |
| --- | --- | --- | --- |
| 1 | `input_tokens` 含 cached，相加会重复计数 | `input = input - cached` | `sessionDetail.js:165` |
| 2 | `reasoning_output_tokens` 是 output 的子集 | 不进 total，仅展示 | `sessionDetail.js:16-18`、`:169` |
| 3 | 误用 `total_token_usage` 当本轮用量 | 只读 `last_token_usage` | `sessionDetail.js:157` |
| 4 | 会话开始/空闲会刷空 `token_count` | `last_token_usage` 为空或全 0 时丢弃 | `sessionDetail.js:158`、`:171` |
| 5 | 工具调用行出现在 `token_count` 之前 | 用 `pendingTools` 攒着，出 turn 时挂靠并清空 | `sessionDetail.js:135`、`:172-173` |
| 6 | IDE 插件会在 prompt 前塞编辑器上下文 | 按 `## My request for Codex:` 切出真实提问 | `sessionDetail.js:67-72` |
| 7 | 三次并发扫描把 tokscale 打到 500% CPU | 刻意串行 | `collector.js:1668-1669` |
| 8 | Codex 不报 cache_write | 恒置 0，不臆造 | `sessionDetail.js:168` |
| 9 | 归档目录容易被漏扫 | `archived_sessions` 同样纳入 | `collector.js:2204` |
| 10 | 自定义 `CODEX_HOME` 可能嵌套在别的 client 目录下 | watcher 的 root 去重逻辑专门处理重叠 | `collector.js:2625-2631` |

---

## 9. 关键文件索引

| 文件 | 职责 |
| --- | --- |
| `src/shared/collector.js` | 调 tokscale、三周期扫描、watch 增量、会话时间戳回补 |
| `src/shared/usage.js` | tokscale JSON → period 聚合（字段兼容、总量口径、多维度累加） |
| `src/shared/sessionDetail.js` | 自研解析 Codex/Claude jsonl，产出每轮 exchange |
| `src/shared/sessionFiles.js` | Codex/Claude 会话文件定位 |
| `src/shared/history.js` | `hasDisjointReasoning()` 白名单（含 codex）与历史曲线口径 |
| `tests/shared/sessionDetail.test.js` | Codex token 口径的回归测试（:54-116） |
| `tests/shared/sessionDetailResolver.test.js` | 跨环境（含 WSL Codex）解析测试（:116-120） |
