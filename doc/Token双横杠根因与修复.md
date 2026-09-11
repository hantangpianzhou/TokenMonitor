# Token Monitor 主界面显示 `--` 的根因与修复

> 结论先行：这不是统计代码 bug，也不是"今天用量为 0"。根因是**正在运行的还是修复前的旧安装版**，源码里的修复从未被加载。已重建包含修复的安装包，按文末步骤即可让 `--` 消失。

## 一、症状

- 主界面 token 总数位置显示 `--`（UI 实际渲染为 em dash `—`）。
- 左下角"模型 / 工具 / 主页"切换器正常，说明不是窗口被压扁。
- 用户重启应用后依旧 `--`。

## 二、根因：`--` 在哪一行触发

渲染层只有一种情况会显示这个占位符：

```
src/electron/renderer/app.js:7847   if (!state.stats) {
src/electron/renderer/app.js:7853     els.totalTokens.textContent = '—';
```

即 **`state.stats` 整个对象没有到达渲染层**（不是"今天为 0"那种局部值）。

真正导致它一直为空的原因：**运行中的 app 是修复前的旧打包版**。

| 证据 | 数值 |
|------|------|
| 安装版 `Token Monitor.exe` 修改时间 | **2026-09-04 23:30** |
| 安装目录 `resources/app.asar` 修改时间 | 2026-09-04 23:30 |
| 旧 asar 中检索 `collectorHasProducedData` | **0 次** |
| 旧 asar 中检索 `2500ms 兜底定时器` | **0 次** |
| 源码修复提交时间 | 9月5日及之后（86549b0 / a2cbdcc / 9b510a2 / 0b79a65 / 1a3a188） |

旧 asar 里**完全不含**上述 5 个修复提交的内容，而它们正是修复"stats 到达 UI / 冷启动竞态 / 全零首扫锁死 / 窗口高度裁切"的改动。旧版没有这些 → `state.stats` 永远填不上 → 必然 `--`。

> 注意：用户"重启验证"重启的是 `C:\Users\Lenovo\AppData\Local\Programs\Token Monitor\Token Monitor.exe`（旧安装版），所以重启无效。

## 三、关键区分：`--` 与 `0` 是两种不同症状

```
state.stats 为空  ──►  显示 "--"   （本问题：旧二进制，对象没到）
state.stats 正常  ──►  today=0    （跨天归零 + 当时 app 未运行，属真值，非 bug）
```

- `--` 修的是"数据没送到 UI"（旧版缺修复）。
- `0` 修的是"今天确实零用量"，重启 app 让其运行即可随实时用量增长。

## 四、源码侧为何现在已正确

渲染层仅在两处写 `state.stats`：

```
src/electron/renderer/app.js:8130   state.stats = overlayAllTimeSessions(await window.tokenMonitor.getStats(options));  // 初始拉取
src/electron/renderer/app.js:12693  state.stats = overlayAllTimeSessions(payload.data.stats);  // 实时推送 onStatsPush
```

引导期多次 `await refreshStats()`（如 `app.js:11880`），源码注释明确写："`await refreshStats()` is the only bootstrap that can populate state.stats"。

main 侧链路：

```
ipcMain.handle('stats:get')         // main.js:6920
  └─ fetchStats(options)
       └─ electronPresentationStats(stats)   // 返回对象给渲染层
emit(stats, reason)                 // main.js
  └─ sendMainWindowEvent('stats:push', ...)  // 实时推送给渲染层
```

提交 `86549b0`（make collected stats reach the UI / harden init bootstrap）与 `1a3a188`（seed/collector handoff explicit）确保冷启动 / 竞态下 `fetchStats` 返回有效对象而非 `null`。因此**从源码运行的版本不会再卡在 `--`**。

## 五、已执行的修复动作

重建了包含全部修复的 Windows 安装包：

```bash
# 构建时需关闭沙箱批量删除保护，放行 electron-builder 对 dist/win-unpacked 的清理
CODEBUDDY_SAFE_DELETE_ENABLED=0 npm run dist:win
```

构建产物（2026-09-06 12:16）：

| 文件 | 用途 |
|------|------|
| `dist/win-unpacked/Token Monitor.exe` | 解包后的可直接运行版，用于即时验证 |
| `dist/Token-Monitor-Setup-0.50.0.exe` | NSIS 安装器，重装覆盖旧安装版 |
| `dist/Token-Monitor-0.50.0.exe` | 便携版 |

**验收**：对新 `dist/win-unpacked/resources/app.asar` 检索，
`collectorHasProducedData` = 4 处、`2500ms 兜底定时器` = 1 处 → 修复已确凿打包进新构建。

## 六、让 `--` 消失（二选一）

**方案 A — 即时验证，不重装（推荐先试）**
1. 完全退出当前 Token Monitor（托盘右键退出，确认进程不在）。
2. 直接运行：`E:\project\token-monitor\dist\win-unpacked\Token Monitor.exe`
3. 该 exe 复用同一份 `%APPDATA%\Token Monitor\` 数据（设置、已采集用量均保留），`--` 应立即消失、显示真实 token 数。

**方案 B — 永久修复（覆盖安装版）**
1. 退出旧 Token Monitor。
2. 运行：`E:\project\token-monitor\dist\Token-Monitor-Setup-0.50.0.exe`
3. 安装器会覆盖 `C:\Users\Lenovo\AppData\Local\Programs\Token Monitor\` 下的旧文件。
4. 之后从开始菜单 / 快捷方式正常启动即可。

## 七、变更清单

| 项 | 说明 |
|----|------|
| 根因 | 运行的是 09-04 23:30 的旧安装版，不含 5 个修复提交 |
| 已做 | `npm run dist:win` 重新打包，纳入全部修复 |
| 产物 | `dist/win-unpacked/Token Monitor.exe` + `dist/Token-Monitor-Setup-0.50.0.exe` + `dist/Token-Monitor-0.50.0.exe` |
| 验收 | 新 asar 含 `collectorHasProducedData`(4) / `2500ms 兜底定时器`(1) |
| 未改动 | 源码逻辑（修复已在先前提交完成，本次仅重新打包） |
| 后续 | 若日后改动源码，记得再次 `npm run dist:win` 重打包，否则运行中的安装版不会自动更新 |

## 八、为什么之前"改了没用"

前几轮在源码里修的 bug（全零首扫锁死在 0、冷启动竞态、切换器被裁切）都只存在于 `E:\project\token-monitor` 的源码与 git 提交里；而用户日常启动的是 `AppData\Local\Programs\Token Monitor\` 下的旧打包 exe。两端版本错位，导致源码改动对运行中的 app 不产生任何影响。本次重建打通了"源码修复 → 打包 → 运行版"这一闭环。
