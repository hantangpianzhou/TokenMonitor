# 悬浮窗（球形用量球）+ 托盘右键菜单：实现说明

> 状态：**已实现**。在现有 Windows 托盘图标能力之上，新增一个**常驻置顶的球形悬浮窗**，实时展示「当前使用总量（完整数字，不做 M/K/亿 缩写）」与费用；并在托盘图标**右键菜单**中增加「悬浮窗」显隐切换项，与主窗口显隐相互独立。
>
> 用户明确要求的两点：(1) 球形——用得越多球越大；(2) 直接展示全部数据，不适用 M/K 单位。

---

## 1. 设计要点（与初版计划的差异）

| 项 | 初版计划（已废弃） | 最终实现 |
|----|--------------------|----------|
| 形态 | 圆角矩形卡片 | **球形**：`rounded_full` 圆形，`bg(accent)` 主题色 + 高光点做球面感 |
| 尺寸 | 固定 `200×84` | **随用量增长**：面积 ∝ 用量，`diameter = MIN + (MAX-MIN)·sqrt(tokens/REF).clamp(0,1)`；`MIN=56, MAX=220, REF_TOKENS=200_000_000` |
| 数字 | 紧凑 `12.3M` / `1.2亿` | **完整数字 + 千分位**：`format_int_grouped` → `123,456,789`（不使用 M/K；用户要求直出全量） |
| 窗口选项 | `WindowLevel::AlwaysOnTop` / `transparent` | `WindowKind::PopUp` + `WindowBackgroundAppearance::Transparent`（本 GPUI rev 不暴露 `WindowLevel`/`transparent`/`set_window_visible`/`start_window_drag`，已通过 Win32 HWND 兜底） |
| 数据来源 | 悬浮窗自订阅 `collector.events()` 并重算 | **复用主窗口 `summary`**：`apply_snapshot` 每轮扫描把总量推给球，零额外 DB 查询、不抢事件通道（避免双订阅窃取主窗事件） |
| 拖动 | `start_window_drag` | Win32 `PostMessageW(hwnd, WM_NCLBUTTONDOWN, HTCAPTION)` 伪造标题栏命中（本 rev 无 `start_window_drag`） |

---

## 2. 架构（数据流）

```
                托盘图标右键
                     │
   ┌─────────────────┴──────────────────┐
   │ 隐藏/显示主窗口 (TRAY_CMD_TOGGLE_WINDOW)│  隐藏/显示悬浮窗 (TRAY_CMD_FLOATING)
   │ 纯 Win32: IsWindowVisible →          │  发往 GPUI: async_channel
   │ ShowWindow SW_RESTORE / SW_HIDE      │       │
   └─────────────────┐                  │  tray_rx.recv()
                     │                  ▼       │
                     │        App 级 on_action(ToggleFloatingWindow)
                     │                  │       │
                     │                  ▼       ▼
                     │        ensure_floating_window(cx: &mut App)
                     │          ├─ 已创建 → Win32 ShowWindow 切换可见 + set_floating_visible
                     │          └─ 未创建 → cx.open_window(PopUp/Transparent/无边框)
                     │                      注册 HWND、SetWindowPos(HWND_TOPMOST)、存 weak 到 APP_WEAK
                     │ 主窗口显隐 (纯 Win32，不进 GPUI)
                     ▼
        悬浮窗 FloatingView 实体
          ├─ 复用主窗 COLLECTOR（仅用于关闭时持久化可见偏好）
          ├─ apply_snapshot 推送总量 → set_totals → cx.notify()
          └─ Render：球形 + 完整数字 + 费用；右上「×」关闭（Win32 隐藏 + 写偏好）
```

**关键决策**
- 悬浮窗 = 第二个 GPUI 窗口（`WindowKind::PopUp` 置顶、无边框、透明背景）。所有窗口创建都发生在拥有 `&mut App` 的 App 级 `on_action` 处理器内，规避 GPUI 线程/上下文约束。
- 跨层通信用**全局 channel**（项目既有 `view_tx/view_rx` 同风格）：托盘 Win32 侧只 `send TrayCommand::Floating`，由 `run()` 内的 `cx.spawn` 任务 `recv` 后 `cx.update(|app| app.dispatch_action(&ToggleFloatingWindow))`。
- 主窗口显隐保持纯 Win32（`IsWindowVisible`），不进 GPUI，最小改动。
- 偏好持久化复用 `SettingsRepo`（键 `floating.window.visible`，默认 `true`），与主窗 `set_theme_color` / `set_scan_interval_seconds` 同机制。

---

## 3. 悬浮窗（FloatingView，src/ui/floating.rs）

**窗口选项**（创建于 `ensure_floating_window`）
```rust
WindowOptions {
    window_bounds: Some(WindowBounds::Windowed(bounds)), // WINDOW_SIZE=240.0 透明方窗承载球
    kind: WindowKind::PopUp,          // 置顶、无任务栏项
    focus: false,                     // 创建时不抢焦点
    show: true,
    window_background: WindowBackgroundAppearance::Transparent,
    window_decorations: None,         // 无边框
    ..Default::default()
}
```

**Render**
- 透明全尺寸 `div`，右上角小「×」（`Stateful<Div>` + `StatefulInteractiveElement::on_click`）：点击 → `show_window(hwnd,false)` + `set_floating_visible(false)` + `COLLECTOR.set_floating_window_visible(false)`。
- 球形 `div`：`rounded_full`、`bg(cx.theme().primary)`（跟随主题强调色）、`on_mouse_down(MouseButton::Left, …)` → `start_window_drag(hwnd)` 拖动。
- 内容：完整数字 `format_int_grouped(total_tokens)`（白字，字号随直径 `clamp(11,26)`）+ 费用 `format_cost_usd(cost_micros)`；叠加左上高光圆做球面感。
- 直径 `diameter()`：面积 ∝ 用量，`sqrt` 压缩低段可见增长。

**数据同步（解耦、零额外查询）**
- `FloatingView` 持有 `hwnd`（创建时从 `HasWindowHandle` 取）、`total_tokens`、`cost_micros`。
- `TokenMonitorApp::apply_snapshot` 在 `snap.summary` 存在时，把 `input+output+cache_read+cache_write` 推给已升级的球实体 `f.update(cx, |v, cx| { v.set_totals(total, cost); cx.notify(); })`；实体降级则清空 `self.floating`。**不**另起 `collector.events()` 订阅、不重查 DB。

---

## 4. 托盘右键菜单（src/platform/windows/tray.rs）

`show_context_menu()` 三项**动态标签**（依当前显隐状态）：

| 菜单项 | 命令 | 行为 |
|---|---|---|
| 隐藏主窗口 / 显示主窗口 | `TRAY_CMD_TOGGLE_WINDOW` | 纯 Win32 `IsWindowVisible` → `ShowWindow(SW_HIDE)` / `SW_RESTORE`+`SetForegroundWindow` |
| 隐藏悬浮窗 / 显示悬浮窗 | `TRAY_CMD_FLOATING` | `send_tray_command(TrayCommand::Floating)` |
| 退出 | `TRAY_CMD_EXIT` | `quit_app()`（保持现状） |

新增/调整：
```rust
const TRAY_CMD_TOGGLE_WINDOW: usize = 1; // 原 TRAY_CMD_OPEN 升级为 toggle
const TRAY_CMD_EXIT: usize = 2;
const TRAY_CMD_FLOATING: usize = 3;

#[derive(Clone, Copy)]
pub enum TrayCommand { Floating }   // 仅悬浮窗需跨入 GPUI；主窗显隐/退出纯 Win32

// 全局
static TRAY_CMD_TX: OnceLock<Sender<TrayCommand>>;
static FLOATING_HWND: AtomicIsize = AtomicIsize::new(0);   // 球 HWND，0=未创建
static FLOATING_VISIBLE: AtomicBool = AtomicBool::new(false); // 供菜单标签联动

pub fn init_tray_commands(tx) / register_floating_hwnd(hwnd) / get_floating_hwnd()
pub fn is_floating_visible() / set_floating_visible(v) / send_tray_command(cmd)
```

`WM_COMMAND` 分发：`TOGGLE_WINDOW → toggle_main_window()`（纯 Win32）；`FLOATING → send_tray_command(Floating)`；`EXIT → quit_app()`。

---

## 5. App 侧接线（src/app/mod.rs + src/app/app.rs）

1. `run()` 内（`#[cfg(target_os="windows")]`）：建 `tray_tx/tray_rx = unbounded()`，`platform::init_tray_commands(tray_tx.clone())`，注册 `cx.on_action(|_: &ToggleFloatingWindow, cx| ensure_floating_window(cx))`，并 `cx.spawn` 任务在 `tray_rx.recv()` 循环中 `cx.update(|app| app.dispatch_action(&ToggleFloatingWindow))`。`cx.spawn` 返回的 `Task` 存入 `APP_WEAK` 指向的 `TokenMonitorApp.tray_task` 字段（保证监听器常驻）。
2. `TokenMonitorApp::new` 内：把 `Arc<Collector>` 写入 `COLLECTOR`（`OnceLock`）、把 `weak_self` 写入 `APP_WEAK`（`OnceLock`）——供球与 tray 跨层取用。
3. `ensure_floating_window(cx: &mut App)`（`#[cfg(windows)]`，位于 `app.rs` 末尾）：
   - `get_floating_hwnd() != 0` → 仅 Win32 切换可见（`show_window` + `set_floating_visible`）。
   - 否则 `cx.open_window(...)` 创建，闭包内取 HWND → `register_floating_hwnd` + `set_always_on_top(true)`，返回 `WindowHandle` → `handle.entity(cx)` 取 `Entity<FloatingView>` → `APP_WEAK.update` 存 `app.floating = Some(weak)`。
4. 启动即显：主窗创建后若 `COLLECTOR.floating_window_visible()`（默认 `true`）则 `ensure_floating_window(cx)`。
5. 平台兜底（`src/platform/windows/mod.rs`，`#[link(user32)]`）：`set_always_on_top(hwnd,on_top)`（`SetWindowPos HWND_TOPMOST`）、`show_window(hwnd,visible)`（`ShowWindow`）、`start_window_drag(hwnd)`（`PostMessageW WM_NCLBUTTONDOWN/HTCAPTION`）。

**特征门控**：悬浮窗全部 `#[cfg(target_os="windows")]`，且 `FloatingView` 模块 `src/ui/floating.rs` 仅在 `ui` feature + windows 编译（`pub mod app` 与 `pub mod ui` 均 `#[cfg(feature="ui")]`）。`cargo check --bins --no-default-features --features tui` 不受影响（已验证 TUI 依赖图无 gpui）。

---

## 6. 涉及文件与改动清单

| 文件 | 改动 |
|------|------|
| `src/ui/floating.rs` | **新增**：`FloatingView` 球形实体（Render + 拖动 + 关闭 + 完整数字展示）；`format_int_grouped` 已在 `src/format.rs` 新增 |
| `src/ui/mod.rs` | 新增 `#[cfg(target_os="windows")] pub mod floating;` |
| `src/format.rs` | 新增 `format_int_grouped(v: u64) -> String`（千分位，全量展示） |
| `src/platform/windows/tray.rs` | `TrayCommand{Floating}` / `init_tray_commands` / `register_floating_hwnd` / `get_floating_hwnd` / `is_floating_visible` / `set_floating_visible` / `send_tray_command`；`show_context_menu` 改 3 项动态标签；`WM_COMMAND` 增 `TRAY_CMD_FLOATING`；原 `TRAY_CMD_OPEN` 升级为 `TRAY_CMD_TOGGLE_WINDOW` |
| `src/platform/windows/mod.rs` | 新增 `set_always_on_top` / `show_window` / `start_window_drag`（Win32 HWND 兜底） |
| `src/platform/mod.rs` | 重导出 `init_tray_commands` / `get_floating_hwnd` / `is_floating_visible` / `register_floating_hwnd` / `set_floating_visible` / `send_tray_command` / `start_tray` / `TrayCommand`（仅 windows） |
| `src/app/mod.rs` | 创建 tray channel、`init_tray_commands`、注册 `on_action(ToggleFloatingWindow)`、`cx.spawn` 监听任务并把 `Task` 存回 App；启动按偏好 `ensure_floating_window` |
| `src/app/app.rs` | `COLLECTOR` + `APP_WEAK` 全局（`OnceLock`）；`floating` / `tray_task` 字段（windows）；`apply_snapshot` 推送总量给球；新增 `ensure_floating_window(&mut App)` |
| `src/app/actions.rs` | 新增 `ToggleFloatingWindow` 动作（沿用 `Quit` 写法） |
| `src/collector/mod.rs` | 新增 `floating_window_visible()` / `set_floating_window_visible(bool)`（复用 `SettingsRepo`，键 `floating.window.visible`，默认 `true`） |

---

## 7. 验证

```powershell
cargo fmt --check            # 通过
cargo check --all-targets    # 通过（UI 全量）
cargo check --bins --no-default-features --features tui   # 通过（TUI 无 gpui 泄漏）
cargo tree -d                # 单一 gpui / gpui-component 来源（无 dual rev）
cargo test                   # 通过
```

## 8. 验收口径（人工目测）

- [x] 托盘右键弹出 3 项：「隐藏/显示主窗口」「隐藏/显示悬浮窗」「退出」。
- [x] 悬浮窗常驻置顶，球形随用量增大；展示完整 token 数字（千分位）+ `$` 费用；数据随扫描刷新。
- [x] 悬浮窗可拖动；「×」关闭后菜单标签变「显示悬浮窗」；再次点击可重开。
- [x] 主窗口隐藏/显示不影响悬浮窗；反之亦然。
- [x] 偏好持久化：重启后悬浮窗显隐状态一致。
- [x] `cargo check --no-default-features --features tui` 通过（TUI 不被殃及）。
