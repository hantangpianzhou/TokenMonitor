# CodeArts Doer 本地 Token 统计 —— 解析工作方案

## 一、目标

对 codeartsdoer（华为云码道 CodeArts Doer 智能体）本地产生的 LLM token 做统计与监控，产出按会话/日期/模型/输入输出维度的用量，支持总量、趋势与限额告警。

## 二、架构与技术栈（已实测确认）

| 项 | 说明 |
|---|---|
| 载体 | IntelliJ IDEA 2026.1 插件 `CodeArts_Agent_223-253_...`，位于 `%APPDATA%\JetBrains\IntelliJIdea2026.1\plugins\` |
| 底层框架 | opencode（`package.json` 依赖 `@opencode-ai/plugin`） |
| 本地服务 | webserver 进程 `http://127.0.0.1:33586`（`webserver-26.8.202-*.exe`），带鉴权，直接访问返回 401 |
| Token 计数 | 运行时由 `jtokkit-1.1.0.jar`（tiktoken Java 版）+ `tokenizers-0.33.0.jar` 现算，非固定落盘字段 |
| 存储依赖 | `sqlite-jdbc`、`h2`、`sqlite-dialect` |

## 三、本地数据源盘点（已探查结论）

| 路径 | 结论 |
|---|---|
| 项目级 `.codeartsdoer\turbocontext.db` | **空目录**（名为 .db 实为目录），当前无数据 |
| `.codeartsdoer\.codebase\file-index.db`（2.3MB）| 代码向量索引，与 token 无关 |
| `%APPDATA%\...\app-internal-state.db`（4.5MB）| JetBrains 内部状态库，被 IDE 进程独占锁，外部无法读 |
| opencode 标准目录（`~\.opencode`、`~\.local\share\opencode` 等）| 均不存在，CodeArts 已重定向存储位置 |
| `idea.log` 中 "token" 关键字 | 全部是 Windows 权限令牌（elevation），非 LLM token |
| `log\telemetry\open-telemetry-metrics-*.csv` | IDE 通用指标，不含 token |

**核心结论：本地不存在现成的、可直接解析的 token 统计文件。token 数据本身需要先定位其真实存储或接口，再解析。**

## 四、解析工作分解

### 阶段 1 —— 定位 token 数据源（前置，最关键）

三条路径并行推进，按可行性排序：

**路径 A：webserver 本地 API（最可能）**

- 解决 401 鉴权：鉴权令牌来源需排查 webserver 启动参数 / 环境变量 / 配置（`cache.properties`、`mcp\mcpServers.json`）。
- 枚举端点：opencode 服务端常见 `/health`、`/app`、`/session`、`/message`、`/announcement`、usage 相关接口。
- 消息/会话响应体中通常携带 usage 字段。

**路径 B：会话持久化文件**

- 全盘（含 `C:\Users\<用户>\` 与项目 `.codeartsdoer`）关键词检索 `prompt_tokens`、`completion_tokens`、`input_tokens`、`"usage"`，扩展名不限（opencode 常为 `.jsonl`/`.ndjson`，也可能被压成 `.db` 或加密）。
- 关键字节特征：文件头 `SQLite format 3`、JSONL 每行以 `{"` 开头且含 `usage`。

**路径 C：运行时埋点（兜底）**

- 插件含 `opentelemetry-sdk-logs`，可在 webserve 与模型网关之间做代理/抓包，截取 chat.complement 响应中的 usage。
- 若走 LSP/MCP 通道，则在本地代理层打点。

### 阶段 2 —— usage 字段提取与解析

定位到消息记录后，统一抽象出下面的字段（两种常见形态需兼容）：

```jsonc
// 形态1：OpenAI 兼容
"usage": { "prompt_tokens": 123, "completion_tokens": 456, "total_tokens": 579 }

// 形态2：opencode 风格（可能含 reasoning/cache）
"tokens": { "input": 123, "output": 456, "reasoning": 0, "cache": { "read": 0, "write": 0 } }
```

解析要点：

1. 每条 assistant 消息取一次 usage，避免把 user/system 消息重复计入。
2. 明确 input/prompt 与 output/completion 的对应关系。
3. 识别多模型、多 provider 来源（每个 provider 上报字段可能不一致，需做归一化白名单）。

### 阶段 3 —— 清洗与标准化

- 时间戳统一（日志/JSONL 中多为 UTC 或 epoch，需归一到本地时区）。
- 会话 ID 关联（`cache.properties` 中有 `lastestChatId=ses_...`，用于串起同一会话）。
- 去重：同一消息可能被重试/流式多次上报，按 message id 去重。

### 阶段 4 —— 聚合统计

| 维度 | 统计口径 |
|---|---|
| 总量 | sum(input)+sum(output)、sum(total) |
| 趋势 | 按 小时/天 切片 |
| 会话 | group by session id |
| 模型 | group by model |
| 输入/输出比 | output/input |

### 阶段 5 —— 输出与告警

- 报表：CSV/JSON 导出，日/周汇总。
- 告警：单日 token 阈值、单次请求 token 峰值、成本换算（按各模型单价）。

## 五、风险与待确认项

1. **最大风险**：token 数据可能根本未落盘（仅内存/云端），此时本地只能靠阶段 1 的路径 C 埋点捕获。**必须先验证路径 A/B 是否存在可读数据。**
2. app-internal-state.db 被独占锁，如需读取须在 IDE 退出后进行，或走只读镜像副本。
3. token 计数由 jtokkit/tokenizers 现算，若仅存文本消息而无 usage 字段，则解析工作转为"本地重算 token"（用 jtokkit 对消息内容重新计 token），口径可能与官方计费有偏差。
4. webserve 鉴权机制未知，可能不存在稳定开放 API，需逆向。

## 六、推荐最小实现（MVP）

1. 先做"数据源探测脚本"：扫描本地 jsonl/db + 探测 `127.0.0.1:33586` 端点，输出"token 数据是否落盘、在哪、可不可读"的结论报告。
2. 若落盘：写解析器（阶段 2-4），输出日维度 CSV。
3. 若不落盘：转阶段 1 路径 C，做代理埋点采集。
4. 全部完成后接入告警与成本换算。