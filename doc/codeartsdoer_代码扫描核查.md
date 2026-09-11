# CodeArts Doer 本地用量适配器 · 代码扫描核查

> 日期：2026-09-01 · 目的：对本次新增代码做全量重新扫描，确认完整性、接线对称性与规范性

## 1. 扫描范围

本次新增/改动共 **16 个文件**，全部围绕「无鉴权直扫 `opencode.db`」方案（仿 `qoderCnUsage.js` 本地适配器范式）：

| 状态 | 文件 | 作用 |
|---|---|---|
| 新增 | `src/shared/codeartsdoerUsage.js` | 本地 SQLite 用量适配器（~493 行） |
| 新增 | `tests/shared/codeartsdoerUsage.test.js` | 3 个确定性测试（路径/override/行归一化，不依赖 DB） |
| 新增 | `assets/icons/codeartsdoer.svg` | `< >` 代码符号图标（mask 用） |
| 新增 | `docs/codeartsdoer_*.md`（4 份） | 数据源盘点 / 集成分析 / 实施规格 / 本地扫描方案 |
| 修改 | `src/shared/clientTracking.js` | `PARSE_LOCAL_CLIENTS` + `KNOWN_CLIENTS` 登记 |
| 修改 | `src/shared/collector.js` | import / watch 根 / 采集块 / history graph / anchor fingerprint / exports |
| 修改 | `src/shared/clientHealth.js` | `KNOWN_SOURCE_CHECK_IDS` 加 `codeartsdoer-db` |
| 修改 | `src/shared/usage.js` | `normalizeClientName` 加 `codeartsdoer` 分支 |
| 修改 | `src/shared/anchorSeed.js` | 透传 `codeartsdoerDbPath` 到 anchor 信任与 fingerprint |
| 修改 | `src/electron/discordRpc.js` | CLIENT 列表 + label |
| 修改 | `src/electron/renderer/app.js` | `clientLabels` / `clientsWithIcon` / `KNOWN_CLIENTS` |
| 修改 | `src/electron/renderer/styles.css` | `.row-icon-codeartsdoer` mask 规则 |
| 修改 | `src/electron/renderer/themePresets.js` | 列表 + label |
| 修改 | `src/electron/renderer/usageCharts.js` | 图表配色 `#2F6BFF` |
| 修改 | `worker/src/shared/{clientHealth,usage}.js` | 经 `sync:worker` 自动生成（见 §4） |
| 修改 | `README.md` | 新增「CodeArts Doer (local adapter)」小节 |

## 2. 接线对称性核对（对照 qodercn）

`qodercn` 是既有本地适配器先例。逐文件核对 codeartsdoer 是否镜像了每个接线点：

| 文件 | qodercn 接线点 | codeartsdoer 对应 | 状态 |
|---|---|---|---|
| `clientTracking.js` | `PARSE_LOCAL_CLIENTS` (L8) | 同 L8 | ✅ |
| `clientTracking.js` | `KNOWN_CLIENTS` 镜像 (L36) | 同 L39 | ✅ |
| `collector.js` | import (L47-52) | import (L54-59) | ✅ |
| `collector.js` | `includesQoderCn` (L1507) | `includesCodeartsdoer` (L1508) | ✅ |
| `collector.js` | readState 初始化 (L1516-1519) | (L1521-1524) | ✅ |
| `collector.js` | periods 变量 (L1540-1543) | (L1544-1547) | ✅ |
| `collector.js` | progress 合并 (L1551-1552) | (L1553-1554) | ✅ |
| `collector.js` | 采集块 (L1595-1642) | (L1620-1642) | ✅ |
| `collector.js` | `freshPartitions` (L1696) | (L1696-1700) | ✅ |
| `collector.js` | 全量合并 (L1708-1711) | (L1769-1773) | ✅ |
| `collector.js` | history graph (L1940-1941) | (L1960-2026) | ✅ |
| `collector.js` | `onXHistoryGraph` (L1909) | (L2025-2026) | ✅ |
| `collector.js` | watch 根 `add('qodercn')` (L2434) | `add('codeartsdoer')` (L2437-2441) | ✅ |
| `collector.js` | `configFingerprint` (L3225) | +`codeartsdoerDbPath` (L3230-3237) | ✅ |
| `collector.js` | `qoderCnDbPathForClients` (L3249) | `codeartsdoerDbPathForClients` (L3249) | ✅ |
| `collector.js` | `collectorAnchorTrust` 透传 (L3267) | (L3267-3270) | ✅ |
| `collector.js` | exports (L4300) | 同 L4300 | ✅ |
| `clientHealth.js` | `qodercn-db` 白名单 (L202) | `codeartsdoer-db` (L203) | ✅ |
| `usage.js` | normalize 分支 (L193) | (L194) | ✅ |
| `anchorSeed.js` | `qoderCnDbPathForClients` (L3) | `codeartsdoerDbPathForClients` (L3) | ✅ |
| `discordRpc.js` | 列表 + label (L11,18) | 同 | ✅ |
| `renderer/app.js` | `clientLabels`/`clientsWithIcon`/`KNOWN_CLIENTS` (L3,16,76) | (L3,16,77) | ✅ |
| `renderer/styles.css` | `.row-icon-qodercn` (L3424) | `.row-icon-codeartsdoer` (L3425) | ✅ |
| `renderer/themePresets.js` | 列表 + label (L65,96) | (L65,97) | ✅ |
| `renderer/usageCharts.js` | 配色 (L369) | `codeartsdoer: '#2F6BFF'` (L369) | ✅ |
| `worker/.../clientHealth.js` | `qodercn-db` (L204) | `codeartsdoer-db` (L205) | ✅ |
| `worker/.../usage.js` | normalize (L196) | (L197) | ✅ |
| `README.md` | Qoder CN 小节 | CodeArts Doer 小节 | ✅ |

**结论：qodercn 的每一处源码接线点，codeartsdoer 都有对称实现，无遗漏。**

## 3. 语法校验

全部 12 个源码 JS + 测试文件通过 `node --check`：

```
OK  src/shared/codeartsdoerUsage.js
OK  src/shared/collector.js
OK  src/shared/usage.js
OK  src/shared/clientHealth.js
OK  src/shared/anchorSeed.js
OK  src/electron/discordRpc.js
OK  src/electron/renderer/app.js
OK  src/electron/renderer/themePresets.js
OK  src/electron/renderer/usageCharts.js
OK  worker/src/shared/clientHealth.js
OK  worker/src/shared/usage.js
OK  tests/shared/codeartsdoerUsage.test.js
```

worker 副本在 `sync:worker` 后重新校验：`usage.js` / `clientHealth.js` 均 `OK`。

## 4. 规范性修复：worker 副本必须经 sync 生成

`worker/src/shared/` 是 `npm run sync:worker` 生成的 `@generated` 副本（AGENTS.md 硬约束：**改 `src/shared/`，绝不改副本，然后跑 sync**）。

- **问题**：初版直接手改了 `worker/src/shared/{usage,clientHealth}.js`，违反规范——CI 的漂移检查会打回。
- **修复**：仅保留 `src/shared/` 的改动，重跑同步脚本：

  ```bash
  node scripts/sync-worker-shared.js
  # → Synced 13 shared modules → worker/src/shared/
  ```

- **验证**：重生成后 worker 副本含 `codeartsdoer`/`codeartsdoer-db` 改动，头部为正确 `@generated` header，语法通过。
- `git status` 中 worker 下 6 个文件 `M`（usage/clientHealth/history/limitProviders/limits/hubBuildRegistry）是 sync 重生成的正常结果。

## 5. git 状态澄清（避免误判）

`git status` 共 93 个文件差异，但**只有 codeartsdoer 相关的 16 个文件是本次新增**：

- 其余 80+ 文件 `M`/`D` 均为「本地旧快照 vs `origin/main` (36307e7)」的差异 —— 工作区比远端旧，非本次改动。
- 典型佐证：`tests/shared/clientHealth.test.js` / `clientTracking.test.js` 显示为 `M`，但其 diff **不含任何 codeartsdoer 字样**（是删除了若干旧行），属远端 9/1 提交带来的快照差异。

提交前注意：不要一并提交那 80+ 个旧快照差异；若需对齐远端，应先 `git checkout -- <文件>` 或 `git pull --rebase` 处理，而非把我新增的改动与旧快照差异混在一起 commit。

## 6. 待验证项（不阻塞功能完整性）

| # | 项 | 状态 | 说明 |
|---|---|---|---|
| 1 | 测试套件运行 | 未跑 | 用户要求 skip debugging；`codeartsdoerUsage.test.js` 为确定性测试，可随时单跑 |
| 2 | 跨平台数据路径 | 仅 Windows 实测 | macOS/Linux 是否同为 `~/.local/share/.codeartsdoer/` 待确认 |
| 3 | `Pangu_Doer_in_CodeArts` 模型 | 需容错 | 该模型 `tokens.total` 为 0，归一化需按模型名兜底 |

## 7. 结论

本次新增代码**接线完整、对称、语法通过、规范已修正**。处于「可提交」状态（前提是只提交 codeartsdoer 相关 16 个文件，不要夹带旧快照差异）。额度采集（Limit Provider）仍走 webserver `Basic` 鉴权，与用量两条独立路径，可后续单独推进。
