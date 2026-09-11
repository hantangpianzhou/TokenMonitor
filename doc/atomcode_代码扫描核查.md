# AtomCode 本地用量适配器 · 代码扫描核查（重新扫描）

> 日期：2026-09-01 · 目的：对 AtomCode 全量接入代码做「重新扫描」，逐项核对接线对称性，并补齐上一轮漏接的渲染层钩子

## 0. 本轮重点：重新扫描发现 3 处漏接钩子（已修复）

上一轮已声称在 `app.js` / `themePresets.js` / `discordRpc.js` 三处做了 atomcode 接线，但**重新扫描（对照 codeartsdoer 逐项核对 Set/数组成员）发现其中 3 个钩子实际未落到位**——之前那个独立校验脚本只检查了「三向顺序不变量 + README 计数」，覆盖了 `KNOWN_CLIENTS` 数组，却**没有覆盖渲染层的成员型 Set/数组**，所以脚本 PASS 没能暴露它们。这正是「用户要求重新扫描新增代码」要抓的盲区。

| # | 文件 / 钩子 | 现象 | 后果 | 修复 |
|---|---|---|---|---|
| 1 | `src/electron/renderer/app.js` `clientsWithIcon` Set (L16) | atomcode **不在** Set 内（codeartsdoer 在） | `iconKindFor()` 对 atomcode 行返回 `dot` 而非 `row-icon-atomcode`，图标不显示 | 在 `codeartsdoer` 后追加 `'atomcode'` |
| 2 | `src/electron/renderer/themePresets.js` `VENDOR_ORDER` 数组 (L65) | atomcode **不在** 数组内（codeartsdoer 在） | `buildVendorPalette` 顺序缺 atomcode，vendor label 走兜底 `toUpperCase` | 在 `codeartsdoer` 后追加 `'atomcode'` |
| 3 | `src/electron/discordRpc.js` `KNOWN_CLIENT_ASSETS` Set (L11) | atomcode **不在** Set 内（codeartsdoer 在） | `KNOWN_CLIENT_ASSETS.has('atomcode')` 为 false，Discord RPC 不挂 atomcode 资产 | 在 `codeartsdoer` 后追加 `'atomcode'` |

> 三个钩子均为「成员判定型」（Set / 数组元素），**不改变顺序**，所以不影响三向顺序不变量，但会影响运行时图标与 Discord 资产表现。其余 atomcode 接线点（见 §2）经核对均已正确落地。

## 1. 扫描范围

AtomCode 沿用 codeartsdoer / qodercn 的「无鉴权本地直扫」范式（目录型 jsonl+meta 双源，计数口径见 `atomcode-usage-analysis.md` §4：`input=prompt`、禁用 `model_usage.tokens.{input,cached_input}`）。本次接入共涉及 **13 个改动文件 + 本核查文档**：

| 状态 | 文件 | 作用 |
|---|---|---|
| 新增 | `src/shared/atomcodeUsage.js` | 目录型双源 jsonl+meta 本地用量适配器 |
| 新增 | `assets/icons/atomcode.svg` | 原子轨道风图标（mask 用） |
| 新增 | `.github/assets/tools-icon/atomcode.png` | 64×64 表格图标占位 |
| 新增 | `.github/assets/tools-icon/codeartsdoer.png` | 同步补齐 codeartsdoer 缺的 PNG |
| 修改 | `src/shared/clientTracking.js` | `PARSE_LOCAL_CLIENTS` + `KNOWN_CLIENTS` 登记 |
| 修改 | `src/shared/collector.js` | import / 59 处接线（采集块 / history graph / watch 根 / exports） |
| 修改 | `src/shared/usage.js` | `normalizeClientName` 加 `atomcode` 分支 (L198-199) |
| 修改 | `src/shared/clientHealth.js` | `KNOWN_SOURCE_CHECK_IDS` 加 `atomcode-sessions` (L173) |
| 修改 | `src/electron/discordRpc.js` | `CLIENT_LABELS` + `KNOWN_CLIENT_ASSETS`（本轮补后者） |
| 修改 | `src/electron/renderer/app.js` | `clientLabels` / `clientsWithIcon` / `KNOWN_CLIENTS`（本轮补前者） |
| 修改 | `src/electron/renderer/themePresets.js` | `VENDOR_ORDER` + `VENDOR_LABELS`（本轮补前者） |
| 修改 | `src/electron/renderer/usageCharts.js` | 图表配色 `atomcode: '#E67E22'` (L369) |
| 修改 | `src/electron/renderer/styles.css` | `.row-icon-atomcode` mask 规则 (L3426) |
| 修改 | `worker/src/shared/{usage,clientHealth}.js` | `sync:worker` 生成副本（见 §4） |
| 修改 | `README.md` / `README.zh-TW/zh-CN/ja/ko.md` | AtomCode 标准表格行（codeartsdoer 同步规范化） |
| 修改 | `tests/shared/clientTracking.test.js` / `tests/docs/readmeConsistency.test.js` | 测试契约对齐 |

## 2. 接线对称性核对（对照 codeartsdoer / qodercn）

| 文件 | 先例接线点 | atomcode 对应 | 状态 |
|---|---|---|---|
| `clientTracking.js` | `PARSE_LOCAL_CLIENTS` (L8) | 同 L8 `['proma','qodercn','codeartsdoer','atomcode']` | ✅ |
| `clientTracking.js` | `KNOWN_CLIENTS` (L32-43) | 末项 `'atomcode'` | ✅ |
| `collector.js` | import (L60-66) | 5 个导出全导入 | ✅ |
| `collector.js` | `includesAtomcode` (L1520) | 同 | ✅ |
| `collector.js` | 采集块 (L1668-1678) | today-only anchor 路径 | ✅ |
| `collector.js` | history graph (L2058-2066) | `buildAtomcodeHistoryGraph` | ✅ |
| `collector.js` | watch 根 (L2528-2529) | `add('atomcode', ...['atomcode-sessions', root])`，目录型无占位文件 | ✅ |
| `collector.js` | `clientSourceRoots` (L2612) | `['atomcode-sessions', ~/.atomcode/sessions]` | ✅ |
| `collector.js` | exports (L4389) | `atomcodeDataPaths` 等 | ✅ |
| `usage.js` | normalize (L198-199) | `atomcode`/`atom_code` 分支 | ✅ |
| `clientHealth.js` | 源检查白名单 (L173) | `'atomcode-sessions'` | ✅ |
| `discordRpc.js` | `CLIENT_LABELS` (L18) + `KNOWN_CLIENT_ASSETS` (L11) | 本轮补 `KNOWN_CLIENT_ASSETS` | ✅（修复后） |
| `renderer/app.js` | `clientLabels`(L3) + `clientsWithIcon`(L16) + `KNOWN_CLIENTS`(L82) | 本轮补 `clientsWithIcon` | ✅（修复后） |
| `renderer/themePresets.js` | `VENDOR_ORDER`(L65) + `VENDOR_LABELS`(L98) | 本轮补 `VENDOR_ORDER` | ✅（修复后） |
| `renderer/usageCharts.js` | 配色 (L369) | `atomcode: '#E67E22'` | ✅ |
| `renderer/styles.css` | `.row-icon-atomcode` (L3426) | mask 规则 | ✅ |
| `worker/.../usage.js` | normalize (L201-202) | 同步副本 | ✅ |
| `worker/.../clientHealth.js` | 源检查 (L173) | 同步副本 | ✅ |
| `README*.md` | AtomCode 表格行 | 5 语言各 1 行 | ✅ |

**结论：对照 codeartsdoer 的每一处源码接线点，atomcode 均有对称实现；上一轮漏接的 3 个渲染层成员型钩子已在 §0 补齐。**

## 3. 语法校验

全部关键 JS 文件通过 `node --check`：

```
OK  src/shared/atomcodeUsage.js
OK  src/shared/collector.js
OK  src/shared/clientTracking.js
OK  src/shared/usage.js
OK  src/shared/clientHealth.js
OK  src/electron/discordRpc.js
OK  src/electron/renderer/app.js
OK  src/electron/renderer/themePresets.js
OK  src/electron/renderer/usageCharts.js
OK  worker/src/shared/usage.js
OK  worker/src/shared/clientHealth.js
```
`assets/icons/atomcode.svg`、`.github/assets/tools-icon/{atomcode,codeartsdoer}.png` 均落盘（432 / 432 / 294 字节）。

## 4. 规范性：worker 副本必须经 sync 生成

`worker/src/shared/` 是 `npm run sync:worker` 生成的 `@generated` 副本（AGENTS.md 硬约束）。本轮未改 `src/shared/usage.js` / `clientHealth.js` 源码，仅改 `collector.js`（非 WORKER_SHARED_MODULES 成员），故重跑同步应为「无漂移重生成」：

```bash
node scripts/sync-worker-shared.js
# → Synced 13 shared modules → worker/src/shared/
```

重跑后 worker `usage.js` (L201-202) / `clientHealth.js` (L173) 仍含 atomcode 引用，与 `src/shared/` 一致，语法通过。**严禁手改 worker 副本**——之前 codeartsdoer 即因此被 CI 漂移检查打回。

## 5. 三向顺序不变量 + README 计数

| 校验项 | 结果 |
|---|---|
| `clientTracking.KNOWN_CLIENTS` (CSV, 29) | 末三位 `cherrystudio,lmstudio,atomcode` ✅ |
| `app.js` `KNOWN_CLIENTS` 数组 (ids, 29) | 末三位一致 ✅ |
| 两者顺序完全一致 | ✅ 三向不变量（KNOWN_CLIENTS == rendererClientIds == README 用法行）保持 |
| 5 份 README atomcode 表格行 | 各 1 行 ✅ |
| README 计数声明 | tools 35+ / usage 29+ / limits 21（5 语言一致）✅ |

> 说明：§0 的 3 处修复均为「成员追加」，**不改变任何顺序**，故上述不变量与计数在修复前后均成立。

## 6. 测试契约修正（上一轮）

- `tests/shared/clientTracking.test.js`：将 atomcode 从「默认跟踪」断言中移除（atomcode 是 opt-in，不应在 `DEFAULT_CLIENTS`）；排除列表扩为 `['micode','qodercn','codeartsdoer','atomcode']`。
- `tests/docs/readmeConsistency.test.js`：`supportedToolOrder` / `supportedToolIdOrder` 各追加 `'AtomCode'` / `'atomcode'`（在 LM Studio 后）。

## 7. git 状态澄清（避免误判）

`git status` 差异中，仅 AtomCode 相关的上述 13 文件 + 本核查文档 + 上一轮 codeartsdoer 文件属本次新增；其余大量 M/D 均为「本地旧快照 vs `origin/main` (36307e7)」的差异，**非本次改动**。提交前注意只提交 AtomCode / codeartsdoer 相关文件，不要夹带旧快照差异（必要时先 `git checkout -- <文件>` 或 `git pull --rebase` 对齐远端）。

## 8. 待验证项（不阻塞功能完整性）

| # | 项 | 状态 | 说明 |
|---|---|---|---|
| 1 | 测试套件运行 | 未跑 | 用户要求 skip debugging；确定性测试可随时单跑 |
| 2 | 跨平台数据路径 | 仅 Windows 实测 | macOS/Linux 是否同为 `~/.atomcode/sessions/` 待确认 |
| 3 | 计数口径 | 已按 analysis.md §4 | `input=prompt`，禁用 `model_usage.tokens.{input,cached_input}` |
| 4 | 渲染层图标 | 本轮修复 | 修复后 atomcode 行应显示原子轨道图标而非圆点 |

## 9. 结论

AtomCode 全量接入代码**接线完整、对称、语法通过、worker 副本无漂移、三向不变量与 README 计数一致**。本轮「重新扫描」的价值在于抓出上一轮漏接的 3 个渲染层成员型钩子（`clientsWithIcon` / `VENDOR_ORDER` / `KNOWN_CLIENT_ASSETS`），已修复——此前仅检查顺序不变量的独立脚本无法暴露这类问题，后续新增 client 时应**对照既有 client 逐项核对 Set/数组成员**，而非仅核对顺序。代码处于「可提交」状态（前提是不夹带旧快照差异）。
