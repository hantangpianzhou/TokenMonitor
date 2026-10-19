# TeleAgent 本地 Token 用量分析

> 分析时间：2026-10-19 · 数据源：`~/.local/share/TeleAgent/users/v1_public_2112087119124815872/teleagent.db`（SQLite，message.data JSON 中的 `tokens` 字段）

## 总览

| 指标 | 数值 |
|---|---|
| 用量日期 | 仅 2026-10-19（当天 15:43–15:5x 使用） |
| 会话数 | 3 |
| assistant 消息数 | 39（其中 12 条零 token，为示例会话占位） |
| 输入 token | 201,875 |
| 输出 token | 63,865 |
| 缓存读取 token | 2,114,816 |
| 缓存写入 / 推理 | 0 |
| 计费成本 | 0（所有消息 cost=0） |

## 按模型

| 模型 | 消息数 | 输入 | 输出 | 缓存读取 |
|---|---|---|---|---|
| chat-pro | 33 | 143,813 | 60,179 | 1,975,552 |
| chat-flash | 5 | 55,270 | 2,323 | 139,264 |
| chat-lite | 1 | 2,792 | 1,363 | 0 |

- **chat-pro 是主力**：承担了主会话「查看项目情况」的绝大部分输出（约 60k），输入输出比约 2.4:1。
- **chat-flash** 用于轻量调用，输出极少（平均 ~464/条），输入占比高——典型的一次性大上下文、短回复模式。

## 按会话

| 会话 | 说明 | 用量 |
|---|---|---|
| 智能体招标项目调研 | 示例任务，标题注明「已生成内容不消耗 token」 | 12 条消息全部为 0 token |
| _SYS_MEMORY_DAILY_LOG_ | 系统记忆日志 | chat-lite：in 2,792 / out 1,363 |
| 查看项目情况 | 真实工作会话 | in 199,083 / out 62,502 / cacheR 2,114,816 |

## 观察

1. **缓存命中率极高**：cache_read 是 input 的约 10.5 倍，主会话 30+ 轮多步工具调用中缓存从 55k 累积到 142k，说明长会话上下文复用良好，实际成本远低于无缓存情形。
2. **输出集中**：chat-pro 单条输出超过 9k token 的有 5 条（多为此类「生成报告/长文本」步骤），其余多为几百 token 的工具调用循环。
3. **cost=0**：本地记录未计费，可能经由内部网关（providerID=NewApi）或额度内调用，本库无法反映真实账单。

## 建议

- 若需长期监控，可仿照 `src/providers/` 现有适配器新增 `teleagent` provider：读取上述 SQLite 的 `message` 表，将 `tokens.{input,output,cache}` 归一化为 `UsageRecord`（会话标题可映射到 project/session 维度）。
- 零 token 的示例会话可在 provider 层过滤，避免污染统计。
