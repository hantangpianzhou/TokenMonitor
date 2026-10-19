# TeleAgent Token 用量解析步骤

> 记录如何从本机 TeleAgent 的本地数据中解析出 token 用量（即 `doc/teleagent-token-usage.md` 的数据来源与解析方法），供后续实现 `teleagent` provider 或复现分析时参考。

## 1. 定位数据源

TeleAgent 是 Electron 应用，数据分散在多个目录，与用量相关的只有一处：

| 路径 | 内容 | 是否有用 |
|---|---|---|
| `~/.config/teleagent/` | 配置、插件、skills（含 `skills/.usage.json`） | 否（skills 使用计数，非 token） |
| `~/.cache/TeleAgent/` | modellink 版本缓存 | 否 |
| `~/AppData/Roaming/teleagent`、`TeleAgent/` | 空 / 渲染层缓存 | 否 |
| `~/.local/share/TeleAgent/users/<user_id>/teleagent.db` | **SQLite：session / message / part 等** | **是，唯一数据源** |

`<user_id>` 形如 `v1_public_2112087119124815872`，多用户时遍历 `users/` 下所有子目录。

## 2. 数据库结构

相关两张表（其余 todo / permission / project 等与用量无关）：

- `session(id, project_id, title, time_created, time_updated, ...)`
- `message(id, session_id, time_created, time_updated, data)` —— **token 数据在 `data` JSON 里**，无独立列。

## 3. data JSON 的关键字段

`message.data` 是 JSON 文本，assistant 角色的消息携带 tokens：

```json
{
  "role": "assistant",
  "agent": "opencowork-default",
  "modelID": "chat-pro",
  "providerID": "NewApi",
  "cost": 0,
  "finish": "tool-calls",
  "time": { "created": 1792395818864, "completed": 1792395818864 },
  "tokens": {
    "input": 55511,
    "output": 9055,
    "reasoning": 0,
    "cache": { "read": 0, "write": 0 },
    "total": 0
  }
}
```

注意：

- `role` 取 `data->role`，**不是**独立列；只有 assistant 消息有 `tokens`。
- `modelID` 即模型名（chat-pro / chat-flash / chat-lite），`providerID` 为网关（NewApi）。
- 时间为 **毫秒时间戳**（`time.created` 或表列 `time_created`，两者一致）。
- `cost` 本地恒为 0，不代表真实账单。
- 存在 `tokens` 全 0 的消息（示例会话占位、被中断的调用，`finish` 为 `None`），需按需过滤。

## 4. 解析流程

```
打开 SQLite（建议只读模式 file:...?mode=ro，避免锁住正在运行的 TeleAgent）
  ↓ 遍历 message 表：SELECT session_id, time_created, data FROM message
  ↓ json.loads(data)，仅保留 role == "assistant"
  ↓ 取 tokens.{input, output, reasoning, cache.read, cache.write}，空值按 0 处理
  ↓ 维度聚合：
      · 按模型   → modelID
      · 按日期   → time_created / 1000 转本地日期
      · 按会话   → session_id，join session.title 得可读标题
  ↓ 过滤 input+output+cache+reasoning 全为 0 的消息（占位/中断）
  ↓ 汇总输出总览 / 按模型 / 按会话 三张表
```

核心 SQL 也可直接用 json_extract 下推到 SQLite 完成聚合：

```sql
SELECT json_extract(data, '$.modelID')                AS model,
       SUM(json_extract(data, '$.tokens.input'))      AS input,
       SUM(json_extract(data, '$.tokens.output'))     AS output,
       SUM(json_extract(data, '$.tokens.cache.read')) AS cache_read
FROM message
WHERE json_extract(data, '$.role') = 'assistant'
GROUP BY model;
```

## 5. 可运行的解析脚本

无 sqlite3 CLI 时的最小实现（Python 3.11 自带 sqlite3 模块即可）：

```python
import os, sqlite3, json, datetime
from collections import defaultdict

db = os.path.expanduser(
    '~/.local/share/TeleAgent/users/v1_public_2112087119124815872/teleagent.db'
).replace('\\', '/')
con = sqlite3.connect('file:' + db + '?mode=ro', uri=True)

by_model = defaultdict(lambda: defaultdict(int))
for sid, tc, data in con.execute(
    "SELECT session_id, time_created, data FROM message"
):
    d = json.loads(data)
    if d.get('role') != 'assistant':
        continue
    t = d.get('tokens', {})
    c = t.get('cache', {})
    day = datetime.datetime.fromtimestamp(tc / 1000).strftime('%Y-%m-%d')
    m = d.get('modelID', '?')
    for k, v in (('input', t.get('input', 0)),
                 ('output', t.get('output', 0)),
                 ('cache_read', c.get('read', 0)),
                 ('cache_write', c.get('write', 0)),
                 ('reasoning', t.get('reasoning', 0))):
        by_model[m][k] += v
        by_model[m]['day:' + day] += v

for m, v in by_model.items():
    print(m, dict(v))
```

## 6. 对接 TokenMonitor 的注意事项

> **已实现**：`src/providers/teleagent/`（2026-10-19），以下注意事项均已落实——多用户根发现（`users/*` 展开 + 根 label 命名空间指纹）、`message.id` 去重键、零 token 消息过滤、只读连接（`SQLITE_OPEN_READ_ONLY` 优先，WAL 场景回退 `query_only`）+ `busy_timeout` 容忍 `SQLITE_BUSY`。

若按 `src/providers/` 的 `ProviderSource` 模式新增 `teleagent` provider：

1. `data_dir` 返回 `~/.local/share/TeleAgent/users/*`（可能有多个用户目录）。
2. `scan` 读取每个用户目录下的 `teleagent.db`，按上述流程产出 `UsageRecord`；
   `session.title` 可映射到会话/项目维度，`modelID` 映射到模型字段。
3. fingerprint 建议用 `message.id`（主键，天然满足 UNIQUE 去重）。
4. 过滤零 token 消息与示例会话，避免污染统计。
5. TeleAgent 运行中即持有该库，读取必须用只读 URI 模式并容忍 `SQLITE_BUSY` 重试。

> AI生成
