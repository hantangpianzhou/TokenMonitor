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

---

## 9. 形态变换：拖到左右屏幕边缘 → 方形

> 状态：**已实现**。悬浮球拖动松手时若贴住屏幕**左右边缘**，吸附成圆角方形并做 240ms 过渡动画；拖离边缘松手自动变回圆形。方形与圆球共用同一套卡片几何（同一套发光、刷新图标、排版预算），但方形**明显更小** —— 边长取球径的 `0.62`（最大约 108px，零用量约 70px），且缩小时**外缘原地不动**，不会从刚吸附的边缘往里"退"出一段距离。方形内的数字改用中文单位 —— **最低单位 万**。
>
> **2026-10-12 二次修复**：宿主窗口从固定 360px 改为**跟着卡片尺寸走**（`window_side_for`）。此前卡片只占窗口中间一小块（停靠方形约 60% 是空透明区），这片不可见的区域是有真实副作用的 —— 停靠时窗口有 100 多像素伸到屏幕外，且悬停 / 按下会在离球很远的地方被窗口吃掉。详见 9.7。

### 9.1 触发与形态

| 松手时的位置 | 行为 |
|---|---|
| 卡片自身边缘距**左/右**工作区边缘 ≤ `DOCK_THRESHOLD`(32px @100% DPI) | 吸附：立刻把窗口移到"卡片距边缘 `DOCK_GAP`(10px)"处 → 动画变方形 |
| 其他位置（含屏幕中间、上/下边缘） | 变回圆形（同一动画） |
| 上 / 下边缘 | **不吸附**（卡片贴顶或压任务栏会被读成"被裁掉的窗口"） |

形态由**一个** `morph` 值（0=圆、1=方）驱动：每个着色层（底色 / 脚部阴影 / 顶部高光 / 边光）的圆角、以及 **Win32 区域**的圆角，全部读它 —— 于是"画出来的形状"和"能点到的形状"在动画中途也是同一个形状。

### 9.2 数字口径（`format::format_tokens_wan`）

取**达到的最大单位**（亿 / 千万 / 百万 / 万），尾数 1 位小数、整数省略 `.0`；**仅当**尾数四舍五入会进位到 10 时才升一级（`9,999,999` → `1千万`，而不是 `10百万`）；低于 1万 仍用万、保留 2 位小数 —— 单位永不丢弃，所以任何量级都可横向比较。

| 用量 | 文案 | | 用量 | 文案 |
|---|---|---|---|---|
| 500 | `0.05万` | | 1,000,000 | `1百万` |
| 5,000 | `0.5万` | | 1,234,567 | `1.2百万` |
| 12,345 | `1.2万` | | 9,999,999 | `1千万` |
| 123,456 | `12.3万` | | 12,345,678 | `1.2千万` |
| 999,999 | `100万` | | 99,999,999 | `1亿` |
| 10,000 | `1万` | | 999,999,999 | `10亿` |

未停靠时仍是原来的 `format_int_grouped`（完整千分位数字），两种口径的切换只跟随 `docked`。

### 9.3 几何：两种形态共用一条规则

- 裁剪区域由 `CreateEllipticRgn` 换成 **`CreateRoundRectRgn`**，封装为 `set_window_region(hwnd, diameter, corner_ratio, window_size)`：把"圆角椭圆的直径"取成矩形边长时四角相合、区域退化成椭圆 —— **一次调用同时覆盖圆与圆角方形**。`set_window_circle_region` 保留为 `corner_ratio = 1.0` 的包装（首帧 seed 仍在用）。视图传的是**窗口自身尺寸**（`diameter == window_size`），所以区域就是"整个客户区、带圆角"；只往里收 `CLIP_MARGIN = 4px`，否则区域与客户区完全重合时 `SetWindowRgn` 会退化成"不裁剪"，方形四角会变回可点死区。
- 卡片圆角 = `side * corner_ratio / 2`，`corner_ratio` 从 `1.0`（圆）过渡到 `SQUARE_CORNER_RATIO = 0.36`（圆角半径 = 边长 18%）。**区域用同一个比例**（边长更大 ⇒ 绝对半径也更大），所以任意尺寸下区域的边与角都比卡片多留出同一圈余量 —— 由 `the_region_contains_the_card_and_its_glow` 守住。
- 卡片的**边长也由 `morph` 驱动**：`card_side_for(ball, t) = ball * (1 - (1 - DOCK_SIDE_RATIO) * t)`，`DOCK_SIDE_RATIO = 0.62` —— 方形明显小于圆球（最大 175 → 108.5，零用量 112 → 69.4），因为实心方块的"视觉重量"大于同边长的圆，而贴边的小部件应该是一个标记而不是一块面板。发光、圆角、排版预算（内接正方形）、刷新图标全部随这条边长一起缩，仍是**一套几何**。
- **窗口尺寸跟着卡片走**：`window_side_for(card) = card + 2 * card_pad(card)`，`card_pad = glow 可见半径 - card/2 + CLIP_PAD`（≈ `0.17 * card + 12`）。于是窗口 = 卡片 + 光晕需要的留白，**窗口里没有多余的透明区**（残余的只有那圈 12px 空环，是区域硬边落点所必需）。窗口因而随用量、`morph`、hover 变化而缩放，由 `platform::fit_floating_card_window` 在 `render` 里下发 —— 这是**唯一**从 GPUI 打到 Win32 的尺寸通路。
- **缩放时卡片原地不动**：`fitted_origin` 按锚点改写窗口原点 —— 自由态锚**卡片中心**（球长大/缩小时原地呼吸），停靠态锚**卡片外缘**（方形收缩时贴着屏幕边缘收进去，不会往屏内退 `(ball-square)/2 ≈ 33px`）。吸附时 Win32 按"卡片外缘距边缘 `DOCK_GAP`"定位，之后每次缩放都保持这条边不动，所以**吸附不需要重算**。竖直方向卡片恒居中，故 y 只按尺寸差的一半平移。
- 窗口尺寸会**在拖拽过程中**变化（hover 放大 6%、morph），而拖拽的抓取偏移量是相对窗口原点记录的 —— 所以 `fit_floating_card_window` 在缩放后会同步修正 `DRAG_OFFSET`，否则球会在鼠标下跳一下。

### 9.4 跨层通路：谁贴边、谁变形

```
WM_*BUTTONUP（拖动结束，ball_drag_subclass）
  ├─ dock_and_snap(hwnd)            ← 立即吸附（Win32 同步完成）
  │     GetWindowRect + MonitorFromWindow/GetMonitorInfoW(rcWork)
  │     + FLOATING_CARD_FRAC（GPUI 每帧 render 发布的"卡片/窗口"比）
  │     → SetWindowPos：让"卡片（居中）"的外缘距工作区边缘 10px
  └─ send_tray_command(TrayCommand::Dock(Option<DockEdge>))
        → app/mod.rs 的 tray_rx 循环 → app::set_floating_docked(Option<DockEdge>)
        → FloatingView::set_docked(edge) → morph_to(1.0 / 0.0)

GPUI 侧 render（每次 notify / WM_SIZE）
  ├─ card_d = card_side_for(sphere_d, morph)      ← 形态、数字、图标都读它
  ├─ want   = window_side_for(card_d)             ← 窗口该多大
  ├─ set_floating_card_frac(card_d / win)          → Win32 吸附用的比例
  ├─ fit_floating_card_window(hwnd, want, edge)   → SetWindowPos（锚点见上）
  └─ set_window_region(hwnd, win - 4, corner_ratio, win)  → SetWindowRgn
```

- **吸附在 Win32 侧同步做完**，不放到 GPUI 之后再补位置：松手那一帧球就已贴边，不会出现"先停在边缘外、下一帧才跳过去"。
- GPUI 只接收"要不要方形、吸附在哪一侧"：`DockEdge` 在**进入**停靠时更新（`Some(Left/Right)`），退出时**保留**到回圆动画结束再清掉（`morph_to` 里清）—— 退出过程中缩放仍要锚在同一条边上，而一个遗留到自由态的 edge 会让下一次 hover 把球朝它并**不**靠着的屏幕边缘放大。变形、缩小、外缘对齐、数字口径全部由 `FloatingView` 负责。
- 卡片尺寸通过 `set_floating_card_frac` 发布（沿用刷新图标 hit-rect 的同一手法）：`card / window` ≈ 0.64–0.68，用于把"卡片自身的边"换算成屏幕像素。窗口只比卡片大一圈光晕留白（≈24–42px），所以 `dock_and_snap` 里那个 `margin` 也随之从 ≈100px 缩到几十像素以内 —— 发布值与实际窗口尺寸一致时吸附才是精确的。
- 变形的动画用**自己的帧定时器**（16ms × 240ms，`ease_in_out`）而不是 `with_animation`：GPUI 动画元素只能重设自己的根节点，而这里要同时改多个**兄弟层**加一个原生区域。
- 外缘对齐反过来也保证了**取消停靠的判定**仍然正确：Win32 用"静止球居中"估算卡片外缘，而实际卡片正是被平移到了这个位置，两者重合。

### 9.5 涉及文件

| 文件 | 改动 |
|---|---|
| `src/format.rs` | 新增 `format_tokens_wan`（+ 私有 `trimmed` / `round1`）与 2 组用例 |
| `src/platform/windows/mod.rs` | `set_window_region`（`CreateRoundRectRgn`）、`DockEdge`、`dock_edge_for`、`dock_window_x`、`fitted_origin`、`fit_floating_card_window`（缩放 + 锚点 + 拖拽偏移修正）、`set_floating_card_frac`、`monitor_work_area`、`window_rect`、`move_window`、`dock_and_snap`；子类松手分支发 `Dock` |
| `src/platform/windows/tray.rs` | `TrayCommand::Dock(Option<DockEdge>)`（复用既有 Win32→GPUI 通道，不新开一条） |
| `src/platform/mod.rs` | 重导出上述新符号 |
| `src/app/mod.rs` | tray 循环处理 `Dock` → `app::set_floating_docked(edge)` |
| `src/app/app.rs` | `TokenMonitorApp::set_floating_docked(Option<DockEdge>)` + 自由函数 `set_floating_docked(Option<DockEdge>, &mut App)`（循环侧只拿得到 `App`） |
| `src/ui/floating.rs` | `docked` / `edge` / `morph` / `morph_task` 字段；`set_docked(Option<DockEdge>)` / `morph_to`；`corner_ratio_for` / `card_side_for` / `card_pad` / `window_side_for`；区域带圆角；方形时数字切 `format_tokens_wan`；每帧发布 `set_floating_card_frac` 并下发 `fit_floating_card_window` |

### 9.6 验收口径（人工目测）

- [ ] 把球拖到屏幕左/右边缘松手：立刻贴边并平滑变成圆角方形，数字变成 `1.2千万` 式中文单位。
- [ ] 方形比圆球**明显小一圈**（边长约 0.62 倍），但口径标签、数字、刷新图标都还在、都能读。
- [ ] 变形过程中方块的**外缘一直贴着边缘不动**（不会先缩进去、离边缘远一截）。
- [ ] 方形状态下拖离边缘松手：变回圆形，数字恢复完整千分位，且第一帧不会横向跳一下。
- [ ] 拖到上/下边缘或屏幕中间松手：保持圆形、不吸附。
- [ ] 方形贴在屏幕两侧、距边缘约 10px；悬停放大不会越过屏幕边缘。
- [ ] 动画中途形状与可点击区域一致（方形四角不被裁、圆形不被切边）。
- [ ] 球**周围没有大片空透明区**：窗口只比可见的球 + 光晕大一圈，悬停高亮、按下拖动都只在球附近生效（离球十几像素外就穿过到桌面）。
- [ ] 拖到屏幕右边缘停靠时，球**贴边而窗口几乎不外溢**；来回拖出再拖入，球不会莫名跑到屏幕中间。
- [ ] 口径标签（今日/本月）与刷新图标在两种形态下都在、都可点。
- [ ] 多屏 / 不同 DPI 下贴边距离一致（阈值与间距都按窗口 DPI 缩放）。

### 9.7 跟随卡片尺寸的宿主窗口（2026-10-12）

**症状**：球变成方形后，周围有一大块透明区域；把它放到屏幕右侧时，悬浮窗会跑到屏幕中间。

**取证**（对用户截图做像素分析，`125%` DPI）：窗口 360 逻辑 px = 450 设备 px，而球 + 光晕只有约 177 设备 px —— **窗口约 60% 是空的**。卡片中心相对窗口中心右移 33.7px，正好等于 `(ball - square)/2`，说明位移逻辑本身是对的；问题纯粹在于窗口比它要显示的东西大太多。

**为什么空透明区会带来真实副作用**：

- 停靠右边缘时要把**卡片**贴到边缘，于是窗口左边 = `工作区右 - 10 - card - margin`，而 `margin = (win - card) / 2`。在旧模型里 `win = 360`、`card ≈ 88`（逻辑 px），窗口右边缘会伸出屏幕 **≈126 逻辑 px（约 157 设备 px）**；任何把"屏外窗口"拉回来的动作（系统/窗口管理/重新显示）都会把球一起拽回屏内 —— 看上去就是"球跑到中间去了"。
- `SetWindowRgn` 只裁掉区域之外的点击，区域本身 = 卡片 + 光晕，所以按下的可命中范围比可见的球大一圈；离球很远的地方也会抢走 hover（球会莫名放大）。

**做法**：让窗口尺寸由卡片推导，而不是让卡片在固定窗口里找位置。

> ⚠️ **这条只适用于停靠的方形**。圆形（不贴边）后来被改回**固定窗口**——见 §9.8，那里记录了"让圆形也跟随卡片"造成的球被削掉 20.5% 的回归。

| | 旧 | 新 |
|---|---|---|
| 客户区边长（逻辑 px） | 固定 360 | `card * 1.34 + 24`（含光晕与 `CLIP_PAD`），即 117–258 |
| 卡片在窗口里的余量（逻辑 px / 每侧） | ≈136（截图那次：360 窗口配 ≈88 卡片） | `card_pad` ≈ 24–42（其中 12 是区域硬边所需的空环） |
| 停靠时窗口伸出屏幕（逻辑 px） | ≈126 | ≈ 一个 `card_pad`（且在渲染上是不可见 / 透明的边缘） |
| 卡片位置 | 居中 + `dock_shift_for` 位移 | 居中；由**窗口**平移来实现外缘不动 |
| 球的尺寸上限 | 由窗口反推（`MAX_BALL_FRACTION` / `max_sphere_for_window`） | 不再需要：窗口跟着球走，球不会再"撑破"窗口 |

- 新增纯函数 `fitted_origin(origin, cur, side, frac, anchor)`（可单测）：自由态锚卡片中心、停靠态锚卡片外缘、竖直恒居中。`resizing_the_window_keeps_the_card_where_it_was` 守住这条不变量。
- 拖拽中缩放会同步修正 `DRAG_OFFSET`：`offset -= (new_origin - old_origin)`，抓取点始终留在鼠标下。
- layout 读的是**客户区实测尺寸**，但当实测值 ≠ 本帧想要的尺寸（说明窗口正要被替换，或还没布局完）时按 `want` 布局 —— `SetWindowPos` 是同步的，这一帧画出来时客户区已经是 `want`，于是尺寸切换的那一帧卡片就居中，而不是"先用旧窗口画一帧再跳正"。**（§9.8 已废弃这条判据：实测 ≠ `want` 只说明 DPI 取整或 OS 还没画完，丢弃实测值反而让卡片落到另一个矩形上。）**
- 新增 / 改写用例：`window_wraps_the_card_and_its_glow`、`the_region_contains_the_card_and_its_glow`、`resizing_the_window_keeps_the_card_where_it_was`（平台层）；删除已不存在的 `dock_shift_for` 相关两例与 `ball_is_capped_by_the_real_window`。`cargo test --lib` 188 passed。

### 9.8 修正：圆形**不**跟随卡片尺寸，只有方形收缩窗口（2026-10-12）

**症状（回归）**：上节改完后，**圆形**静止悬浮球的下缘被切掉一大块（截图，未停靠、鼠标悬停在球上）。

**像素取证**（对截图做圆拟合，两处边界都做代数最小二乘）：

| 测量项 | 数值 |
|---|---|
| 球直径（130 点，残差 max 0.45px） | **171.15**（圆心 `(133.95, 156.82)`） |
| 裁剪区直径（236 个硬切点，残差 max 0.64px） | **211.50**（圆心 `(118.00, 106.33)`） |
| 两心偏移 | **52.94px**（dx 15.95 / dy 50.49） |
| 球外缘超出裁剪区 | 32.7px |
| **被切面积** | **4737 px² = 球面积的 20.5%** |
| 光晕四向余量 | 左 27.3 ✓ / 上 27.2 ✓ / 右 **4.3 ✗** / 下 **−30.5 ✗** |

- 球径 171.15 = `sphere_diameter_for(120,459,268, hovered=true)` = 160.9 × 1.06 ⇒ 截图时鼠标悬停在球上；数字未切中文单位 ⇒ `docked = false`。
- 裁剪边界残差仅 0.64px、是**完美圆** ⇒ 出自 `CreateRoundRectRgn`（`corner = d` 退化为内接圆），不是 GPUI 的矩形 mask。

**根因**：裁剪区**恒以客户区为中心**（`left = (cw − d)/2`），而卡片用 `ball_box(win_w, win_h, card_d)` 定位 —— 两者是同一个矩形才同心。上节把窗口改成"跟随卡片"后，`win_w/win_h` 一旦走到 `viewport_size()` / `(want, want)` 兜底分支，卡片就落在**另一个**矩形上，与裁剪区错开 ⇒ 切掉一侧。触发它的是上节新加的 `fits` 判据：

```rust
let fits = |w: f32, h: f32| w.min(h) >= MIN_SANE_WINDOW && (w.min(h) - want).abs() <= 0.5;
//                                                        ^^^^^^^^^^^^^^^^^^^^^^^^^ 新增
```

客户区只要 ≠ `want` 就被整对丢弃。HEAD 的 `fits` 只有前半段，接受任何合理客户区，所以球总是居中。

**修正后的模型**（`window_side_for(card_d, t)` 两端 + 区域由卡片推导）：

| | 圆形（`t = 0`） | 方形（`t = 1`） |
|---|---|---|
| 窗口客户区 | **固定 `WINDOW_SIZE`(360)**，从不 resize | `clip_diameter_for(card)` = `card * 1.34 + 24` |
| 区域直径 | `clip_diameter_for(card)`（= 旧的 `clip_diameter_for(sphere_d, win)`，数值完全相同） | 同左（= 窗口 − `CLIP_MARGIN`） |
| 为什么 | 圆从不贴边，空余无害；**不 resize 才能让卡片与区域天然同心** | 方形要贴边，必须去掉透明区 |
| 与 HEAD 的关系 | **逐位等同** | 上节的新行为，保留 |

- 新增纯函数 `clip_diameter_for(card_d)` = 旧的 `clip_diameter_for(sphere_d, win)` 去掉 `.min(...)`：区域**只由卡片推导，永不由窗口推导** —— 于是"区域一定包住画出来的东西"是构造性的，窗口尺寸再怎么异常也只会退化成"漏出方形角"，不会削掉球。
- 读客户区的顺序改为：**先发布 `card_frac`（resize 前）→ `SetWindowPos` → 再重新测量**（`measured_client`）。`SetWindowPos` 是同步的，所以这一帧的卡片与区域用的是**同一个**矩形 —— 之前"先量后 resize"会让两者差一帧。
- 删掉 `fits` 里的 `want` 比较（回到 HEAD 口径），并把这段判断移进 `measured_client` 并写明理由。
- `region_moved` / `last_region` 从 `(win, ratio)` 变成 `(win, ratio, region_d)` 三元组。
- 新增/改写用例：`the_free_ball_keeps_the_fixed_window`（新）、`window_wraps_the_card_and_its_glow`（改成"区域 = 卡片 + 2×`card_pad`；窗口 ≥ 区域；停靠窗口 == 区域"）、`the_region_contains_the_card_and_its_glow`（用真实 `region` 直径）、`identical_regions_are_not_reapplied`（三元组）。`cargo test --lib` 189 passed。

### 9.9 根因：`SetWindowRgn` 量的是**窗口坐标**，而卡片按**客户区**布局（2026-10-12）

**症状**：§9.8 之后**圆形仍然被裁** —— 卡片右下出现直线切口，刷新图标只露出上半。

**为什么 §9.8 没治好**：§9.8 把回归归因于「客户区 ≠ `want`」时按 `want` 布局。这个诊断**不完整**：真正一直在切球的是一个**与窗口尺寸无关**的坐标系错配，所以即便把圆形改回固定 360 窗口，它照样偏、照样被切。

**取证**（ctypes 直读活窗口，先 `SetProcessDpiAwarenessContext(-4)`；否则读到的坐标被 DPI 虚拟化）：

```
window_rect=(600,234,910,544)   310×310      ← GetWindowRect
client=294×271  origin_in_window=(8,31)      ← GetClientRect + ClientToScreen
window_rgn_box=(38,27,255,244)               ← GetWindowRgn + GetRgnBox（窗口坐标）
```

悬浮窗**不是无边框窗口**：`GetWindowRect − GetClientRect = 16×39`，客户区原点落在窗口内 `(8,31)`。而：

- 卡片布局与区域居中**全部按客户区**表述（`ball_box(win_w, win_h, card_d)`、`left=(cw−d)/2`）；
- `SetWindowRgn` 的坐标是**窗口坐标**（原点 = 窗口矩形左上角）。

二者原点相差 `(8,31)`，区域被原样提交时就整体左上平移这么多 ⇒ **左上多留一圈光晕、右下切穿卡片**。与截图逐项吻合。

- 测这个偏移的唯一可靠办法是 `ClientToScreen(hwnd) − GetWindowRect().left/top`：`GetClientRect` 的 `left/top` **恒为 0**，答不出"客户区在窗口里的哪里"。
- gpui 自己也区分这两者：`gpui_windows/src/window.rs::WindowBorderOffset::update` 就是 `GetWindowRect − GetClientRect` 逐轴差值。这不是本 fork 独有的现象。

**修复**（`src/platform/windows/mod.rs`）：

| # | 位置 | 改动 |
|---|---|---|
| 1 | `set_window_region` | 区域中心 **`+ client_origin_in_window(hwnd)`**（新增 `ClientToScreen` 绑定 + `client_origin_in_window`） |
| 2 | `fit_floating_card_window` | `SetWindowPos` 设的是**窗口矩形**，几何按**客户区** ⇒ 「要 `s` 的客户区」必须设「`s + window_frame_size(hwnd)` 的窗口」（新增 `window_frame_size` = `GetWindowRect − GetClientRect` 逐轴差）；`fitted_origin` 的 `cur` 同样改用**客户区**尺寸 |
| 3 | `dock_and_snap` | `margin` 加上 `origin_x` —— 卡片相对**窗口**左缘的偏移，否则贴边基准整体差一个边框 |

**实测对照**（同一台机、125% DPI）：

| | 修复前 | 修复后 |
|---|---|---|
| free 窗口 / 客户区 | 360×360 / 344×321 | 376×399 / **360×360** |
| 区域 box（窗口坐标） | (42,30,260,260) | (57,80,262,262) |
| 区域中心 vs 客户区中心 | (172,160) vs (180,191.5) ⇒ 偏 **(8, 31.5)** | (188,211) vs **(188,211)** ⇒ **0** |
| 停靠 `Right` | — | window 189×212 / client 173×173 / rgn_box (10,33,168,168) ⇒ 区域中心 (94,117) vs 客户区 (94.5,117.5)，偏 **0.5px** |

→ **区域与客户区同心**（残差 ≤ 0.5px，纯 DPI 取整），"区域包围卡片"由构造保证。视觉上：free 圆光晕四向均匀、无切口；停靠方形四角圆角齐整、刷新图标整枚可见。

**新增诊断**：`dock_debug` —— 设 `TOKENMONITOR_FLOAT_DEBUG` 时把一行 `drop: window=(…) client=… origin_x=… frac=… card=… margin=… edge=…` 追加到 `%TEMP%\tokenmonitor-dock-debug.log`。GUI 进程的 `stderr` 看不到，落文件才拿得到数。

**遗留（小）**：停靠贴边间隙实测 ~7px 而非 `DOCK_GAP`=10px —— `dock_and_snap` 读的是**松手那一刻**发布的 `frac`（此时 `morph=0`，按圆尺寸 179px 估卡片），而落定后 `morph=1`、卡片缩到 ~103px，估算偏大 ⇒ 贴边偏保守。修法：平台层按「目标形态」推算，或 morph 落定后重 snap 一次。本轮未动。

> **带走的结论**：跨 Win32 ↔ GPUI 边界的所有几何，都要先问清楚**是哪个矩形** —— 窗口矩形、客户区、还是屏幕工作区。本次三处修复全是同一个问题在三个地方的表现。

### 9.10 修贴边间隙：`fitted_origin` 要两个分数，且必须先 resize 后发布（2026-10-12）

**症状**：§9.9 修完裁剪后，停靠态卡片与屏幕边缘的间距只有 ~7px，而不是 `DOCK_GAP` 的 10px。

**逐帧取证**（在 `fit_floating_card_window` 里临时加 `dock_debug` 逐帧 dump，定位后即删）：

```
drop: card=180.2 margin=97.9 card_x=1221.9..1402.1 work=(0..1366) edge=Right   ← 落点 1356.1 = 1366−10 ✓
fit #1: cur_client=360 want=356 frac_from=0.4926(本帧卡片177.3) frac_to=0.4981 → dx=2
fit #2: cur_client=356 want=333 frac_from=0.4607(本帧卡片164.0)                → dx=11.5
fit #3: cur_client=333 want=278 frac_from=0.4252(本帧卡片141.6)                → dx=27.5
```

吸附落点本来就对。问题出在 morph 动画的每一帧：`dx = cur_r − new_r` 的数学没错，**但 `frac_from` 取的是「本帧新卡片」在旧窗口里的占比，不是「此刻屏幕上的卡片」**。卡片每帧缩小，于是每帧都在追一张已经缩过的卡片，右缘每帧内移半个收缩量 —— 6 帧累计 **34.5px**。

**两处修复**（都作用在 `fitted_origin` 的输入上）：

| # | 位置 | 改动 |
|---|---|---|
| 1 | `fitted_origin` | 新增 `frac_to` 参数：`frac_from`（离开的窗口上的卡片）与 `frac_to`（目标窗口上的卡片）分开。morph **同时**改卡片与窗口，单值必然错 |
| 2 | `ui::floating::render` | **把 `set_floating_card_frac` 移到 `fit_floating_card_window` 之后** —— 真根因：发布顺序错，`fitted_origin` 锚到的就不是屏幕上的卡片；发布改用 fit 之后实测的客户区 |

| | 修前 | 修后 |
|---|---|---|
| 贴边间距（像素实测） | 44px | **7px**（真实几何 ≈9px，像素测量有 ~2px 渐变边缘误差；目标 10） |
| 卡片右缘漂移（morph 全程） | 34.5px | ≤0.5px |

剩余 1–3px：`dock_and_snap` 只能按「松手瞬间、`morph` 仍为 0 的 hover 卡片(180.2)」估算，而动画第一帧卡片已变(177.3)。要再精确就得在 morph 落定后重 snap 一次，暂不加通路。

**测试**：`the_morph_keeps_the_docked_cards_outer_edge`（单帧，`frac_from ≠ frac_to`）+ `a_run_of_morph_frames_keeps_the_docked_edge`（**链式 6 帧**，同时断言正确顺序不漂移、旧顺序漂移 >5px）。

> ⚠️ **这个 bug 单帧测不出来**：单帧时正确与错误顺序的 `dx` 只差几 px。凡是"本帧要动的量"和"上帧留下的量"可能被同一行代码混用，就必须链式测；累积漂移也只有逐帧 dump 能定位 —— 只看终态数据连方向都看不出来。

### 9.11 动画丝滑：包络冻结 + 帧回调驱动，及包络引入的停靠缝隙回归（2026-10-12）

**症状**：用户反馈「动画不够丝滑」。逐帧计时定位真因：morph 每帧同步 `SetWindowPos` + `SetWindowRgn`，实测 **42–59ms/帧**，而 240ms 过渡的帧预算只有 16ms ⇒ 过渡塌缩成约 5 帧，其余帧被这两次 Win32 结构调用吃掉。

#### 9.11.1 修法一：包络冻结（结构变更从 15 次降到 2 次）

新增 `morph_envelope(sphere_d) -> (f32, f32)`，返回 `(max(WINDOW_SIZE, clip_d), clip_d)` —— 过渡期间窗口与区域钉死在**包络几何**上（`morph_geometry(..., animating)` 在 `animating = true` 时返回包络值），每个过渡只有**起 / 止 2 次**结构变更，而不是每帧一次：

| | 修前（每帧） | 修后（包络） |
|---|---|---|
| `fit_floating_card_window` | ~20–30ms | **0.5–0.9ms**（中间帧无结构变更，纯读测量） |
| `set_window_region` | ~20–30ms | **0.5–0.9ms**（同上） |
| 卡片绘制（GPUI） | <1ms | <1ms（**仍每帧动画**，视觉完全连续） |

卡片本身由 GPUI 绘制、每帧动画，所以冻结的只是宿主窗口与原生裁剪区这两个"昂贵且不可见中间态"的东西。

#### 9.11.2 修法二：帧回调驱动（替掉 16ms 定时器）

`advance_morph` 原先由 16ms `background_executor` 定时器驱动，改为 `window.request_animation_frame()`：

- 定时器唤醒按**自己的 schedule**、与显示刷新率无关：落在 vsync 刚过就要等整个刷新周期 ⇒ 间隔在 16.7 / 33.3ms 间抖动、平均 ~30ms。**这就是抖动，且单独计时 `render` 看不出来**。
- 帧回调是窗口合成所用的同一时钟 ⇒ 一次回调 = 一个显示帧，240ms 过渡拿到它一直预算的 ~14 帧。

**帧率实测**：gpui 对非激活窗口挂帧回调限速 30fps（`crates/gpui/src/window.rs` 非激活窗口 33333µs 节流）；但真实拖动时 125Hz 鼠标输入维持 `InputRateTracker` 高速率（100ms 内 ≥6 个输入事件、之后维持 1 秒）⇒ 覆盖整个 240ms 过渡 ⇒ **拖动中 60fps**；只有无输入运行（探针场景）才掉到 30fps。`is_active()` = `hwnd == GetActiveWindow()`，球窗永不为激活窗口。

#### 9.11.3 修回归：落定矫正（包络冻结的代价）

冻结窗口后卡片改为**居中绘制**（不再每帧重锚），于是尺寸变化时外缘会漂移**半个变化量**：dock 落定内移 ~0.19×sphere（贴边缝隙从 10px 退化到 ~35px）、undock 外移同量。

新增纯函数 + 落定帧一次性平移：

```rust
fn envelope_landing_shift(edge: DockEdge, card_from: f32, card_to: f32) -> f32 {
    let outward = match edge { DockEdge::Right => 1.0, DockEdge::Left => -1.0 };
    outward * (card_from - card_to) / 2.0
}
```

- 落定帧在 `advance_morph` 记账（`landing_shift`），**在本帧 resize 之后**由 render 末尾 `shift_floating_window` 一次性平移复原外缘 —— 放在 resize 前会被重新锚定到漂移后的位置。
- undock 落定前**不清** `edge`（§9.1 的约定）正是为了让这次矫正还知道哪边是外。

#### 9.11.4 端到端验证

合成 `SendInput` 拖动触发不了 dock（单发 ~23ms + 系统合并批量移动，达不到高速输入门槛），故用应用内 `TM_AUTODOCK` 钩子忠实复现下落序列（泊车到右缘 → `dock_and_snap` → `set_docked` → 左移 300px → `set_docked(None)`），取证后随其他临时代码一并删除。

| | 修前 | 修后 |
|---|---|---|
| dock 贴边缝隙 | ~35px（包络回归后）/ 9.8px（§9.10 修后） | **9.8–10.0px**（目标 10） |
| undock 外缘漂移 | ~0.19×sphere | **−0.6 / +0.2px** |
| 过渡帧数 | ~5 帧（塌缩） | dock 12 帧 / undock 15 帧 |

> 探针会在落定帧「先 resize 后 shift」的**中间态**采到 36.8px，属同一帧内的预期中间态，不是回归。

**测试**：新增 `the_landing_shift_restores_the_docked_cards_edge`（dock/undock 对称、左右镜像、零变化零欠账）。全量 `cargo test --lib` **195 passed**；`cargo fmt --check` / `cargo check --all-targets`（零 warning）/ `cargo build` 全绿。

### 9.12 跨 DPI 边界拖出：球被自己的旧区域切角（2026-10-13）

**现象**：从停靠方块拖出后球只剩一小块（直边 + 圆弧的残片，球心区域全黑），再拖一下才恢复正常。

**根因（结构性，与 morph 无关）**：

球与区域各自跟踪**不同的矩形**：

- 球恒画在 `measured_client`（真实客户区，`GetClientRect` ÷ scale）的中心 —— 跟踪**当前**客户区；
- 区域恒在**裁切时刻**的客户区中心与设备尺寸上（`set_window_region` 自读 `GetClientRect`，直径 = `round(min(client_w, client_h) × frac)`）—— 跟踪**过去**的客户区。

只要裁切后客户区变了而没重裁，两者立即偏心。而重裁条件 `region_moved` 只比较**逻辑三元组** `(win, ratio, region_d)`，它按设计 DPI 无关 ⇒ **DPI 变更改变客户区设备几何、三元组一个不变 ⇒ 不重裁**。

DPI 变更的唯一外部路径是 gpui `handle_dpi_changed_msg`（`events.rs:857-928`）：非最大化窗口**立即**改 `scale_factor`，再按系统建议 rect `SetWindowPos`（同步发出 WM_SIZE/WM_MOVE）。gpui 全仓不碰 `SetWindowRgn`，所以区域只剩我们重裁一条路。用户主/副屏 scale 不同（副屏 125%），从副屏右缘的停靠方块拖出必然跨边界。

两个让 bug"可见且持续"的帮凶（均已代码核实）：

- gpui `bounds_changed`（`window.rs:2394-2400`）在**每次 resize/move 后都 `refresh()`** ⇒ DPI 变更后必有 render：球被重画进**新**客户区，区域还是**旧**设备几何 ⇒ 当场切角；拖拽中每次 `move_window`→WM_MOVE 也都触发 render。
- `set_docked` 的**同边 no_op 早退**（floating.rs:920）不 morph、不 notify ⇒ 若二次撒手仍落同一边缘，没有任何东西会重裁 ⇒ 切片状态持续存在。只有拖到别处撒手（真正改变停靠状态）触发 morph 改三元组才自愈 —— 与用户"再拖一下才正常"完全吻合。

**修法：重裁判据补全为「裁切所依据的客户区设备矩形」**：

```rust
// platform：set_window_region 现在返回它实际依据的客户区设备尺寸
// （内部回退值；未裁成则 (0,0)），并新增 client_size_device(hwnd)。
pub fn set_window_region(hwnd, diameter, corner_ratio, window_size) -> (i32, i32)

// view：新增 last_clip_client，render 中
if let Some(clip_client) = client_size_device(hwnd) {
    if clip_client != self.last_clip_client || region_moved(self.last_region, region) {
        self.last_clip_client = set_window_region(hwnd, region_d, corner_ratio, win);
        self.last_region = region;
    }
}
```

- 客户区**设备矩形**是裁剪函数的直接输入（直径比例 + 居中基准），把它并入 memo 后，任何外部几何变化（DPI 变更、OS 钳制的 resize、frame 变化）都在下一帧重裁；`set_window_region` 本就按当前客户区居中 + 按比例取直径，**一次重裁同时修好直径与偏移**。
- 客户区不可信（`None`）时**跳过裁剪**而非拿猜测矩形裁 —— 区域永不可能去描一个卡片没被布局进去的矩形；种子区域保证首帧仍是圆的。
- 逻辑三元组与设备矩形合起来即裁剪函数的**完整输入集**（scale 变必改其一）， memo 从此完备。
- 拖拽中的瞬时 scale 分歧（`GetDpiForWindow` 已新、gpui `scale_factor` 仍旧，WM_MOVE 驱动的 render）不会切片：那些帧里球与区域都还是旧缩放几何，WM_DPICHANGED 落地后自愈。

**验证**：`cargo fmt --check` 干净；`cargo check --all-targets` 零代码告警；`cargo test --lib` **195 passed**；`cargo build --bin tokenmonitor-app` 成功。真机跨屏拖出的复现验证待用户确认。

### 9.13 复发与真根因：裁球的不是区域，是客户区矩形（2026-10-13）

**现象复发**：§9.12 的修复上线后，同一操作仍复现（第三张截图）。新证据推翻"区域陈旧"的主判断：

- 残片仍是四分之一圆盘（64×64，球心在裁剪角右下方 ~19px，球径 ~128 设备 px ≈ 102 逻辑 px = 停靠态卡）；第一张截图残片 17×17、球心**恰在**裁剪角。
- **裁剪角是锐利直角**。区域若是圆角矩形，角半径 = `d × ratio / 2` ≈ 26px，残片会明显伸过直角；实测角锐到 1px 内 ⇒ **裁球的不是 Win32 区域，是 OS 对绘制的裁剪（客户区/窗口矩形）**。
- 球心落在客户区右下角 ⟺ **布局框约为真实客户区的两倍**：球恒画在布局框中心（`ball_box` + `measured_client`），裁剪角 = 客户区角，两者重合只可能是布局框中心顶到了客户区的角。

**真根因**：`fit_floating_card_window` 的"已是目标尺寸"判据只看**窗口矩形**（`cur == want_client + frame`）。而 frame = `GetWindowRect − GetClientRect` 是**两个活动矩形的差**，OS 并不在同一瞬间更新它们（DPI 变更先改窗口矩形，非客户度量随后追上），`WM_DPICHANGED` 的建议 rect 又会在拖球跨屏时**在背后**改窗口尺寸。于是窗口矩形量起来"已经对了"、函数早退，而**客户区并不是卡片布局进去的那个矩形**——球按 ~2× 的布局框画，被 OS 裁进小客户区，只剩一角。§9.12 的区域备忘录覆盖的是另一条路径（区域陈旧），管不到这条，所以复发。

**修法（两处，都不动几何不变量）**：

1. `fit_floating_card_window` 改为以**客户区**为收敛目标：测量客户区 → 按精确差额 `SetWindowPos` → 再测量，每轮把差值清零，最多 4 轮（常态第一轮即命中，resting 帧全 no-op）。DPI/frame 每轮重读——resize 自身就可能递送改变它们的 `WM_DPICHANGED`。
2. `measured_client` **删掉 `window.viewport_size()` 回退**：GPUI 的视口是它对窗口的"信念"，OS 应用 resize 期间可以是真实客户区的两倍——正是本 bug 的形状。客户区读不到时改用 view 的 `last_client`（上一次可信客户区，逻辑 px，DPI 无关），丢一帧陈旧布局，永不拿一个卡片没被布局进去的矩形作数。

**取证设施（flag 文件门控，默认零开销）**：`%TEMP%\tm-frame.on` 存在即向 `%TEMP%\tm-frame.log` 追加——render 每帧的 morph/animating/want/card/client/region/last_clip/device/region_box，外加子类里 `WM_MOVE/WM_SIZE/WM_WINDOWPOSCHANGING/WM_WINDOWPOSCHANGED/WM_DPICHANGED` 的 window/client/region 三元组。静态球不渲染时消息侧仍能记下 OS 背后改窗口的瞬间——这是"猜测不同步"与"眼见为实"的区别。

**验证**：`cargo fmt --check` 干净；`cargo check --all-targets` 零代码告警；`cargo test --lib` **195 passed**；`cargo build --bin tokenmonitor-app` 成功。真机跨屏拖出复现待确认；若仍有异常，`tm-frame.log` 可直接钉死帧序列。

### 9.14 再复发：布局框信了"记忆"，球被裁成 24px 碎片（2026-10-14）

**现象**：从停靠方块拖出后，球没有恢复成大圆，而是只剩左上角一小块（用户观感"变得更小了"），要拖一下才恢复。

**取证（本轮结论由实时进程转储 + 截图几何复原得出，不再是先验推断）**：

- 截图 376×399 = 球窗口 100% DPI 尺寸（360 客户区 + (16,39) 边框）；残片 24×24，位于左上。
- 截图左/上是**锐利直切口**，切口位置 (144,167) 换算成屏幕坐标 = **(1366,768) = 主屏右下角**；球心 (1368.6, 770.6) 恰在其右下 2.6px ⇒ **残片 = 球心左上的 1/4 圆盘被 L 形监视器缝切剩的一角**。
- 残片外弧最小二乘拟合 r=23.2、残差 0.42px（锐利，非高斯衰减）⇒ 该弧是**停靠态圆角方区的角半径**（`0.36 × d/2`，d≈129 设备 px）⇒ 切球用的确实是**停靠尺寸的客户区矩形**。
- 实时转储运行中的进程（`GetWindowRect/GetClientRect/GetDpiForWindow/GetWindowRgn/GetRgnBox`）证实：当前状态是健康的（client 360×360、region 203×203 同心）——**坏帧不会自愈，因为静态球不重渲染**，"再拖一下才正常"= 拖动触发一次重渲染。

**真根因（三类症状同根：布局框 / 客户区 / 区域裁剪，三方信任了不是 OS 裁剪所用的那个矩形）**：

1. `render` 的布局框回退是 `measured.unwrap_or(self.last_client)`，而 `last_client` 是**自由态最后一次测量值 360**。`measured_client` 在 DPI 跨屏迁移 / 拖动 / morph 期间 `GetClientRect` 瞬态不可读（或 scale≤0/NaN、客户区瞬时 < `MIN_SANE_WINDOW`）时返回 `None` ⇒ 布局框停在一个**早已作废的 360**。
2. 与此同时 OS 已把真实客户区收敛到**停靠尺寸**（~177 设备 px ≈ 141.6 逻辑 × 1.25 DPI）。球恒画在布局框中心 ⇒ 球心顶到真实客户区的**角**上，被裁剩 1/4。
3. `set_window_region` 还有一条 **phantom 回退**：`GetClientRect` 读不到时按 `window_size × GetDpiForWindow/96` 幻象客户区定心。它与 (1) 叠加时把区域中心放到客户区中心外 ~133px，等于把球心推出裁剪矩形更远。

**修法（三处，全部"对一切候选触发免疫"，不依赖猜中是哪一条）**：

| # | 位置 | 改动 | 治本点 |
|---|------|------|--------|
| 1 | `ui::floating::render` | 删 `last_client` 字段；回退改为 `measured.unwrap_or((want, want))` | 布局框**向前**信任"本帧刚要求 OS 的尺寸"，绝不向后信任记忆。向前错最多错一帧未落地的 resize；向后错能错掉一整个停靠态 |
| 2 | `platform::windows::fit_floating_card_window` | 签名加 `scale: f32`（用 gpui 的 `window.scale_factor()`，不再 `GetDpiForWindow`）；客户区测不到时**直接 return 不动窗口** | 拖球跨屏时 gpui 先应用新 scale、`WM_DPICHANGED` 的建议 rect 后到，用旧 DPI 算 `want_client` 正是"布局框是真实客户区的一个分数"的来源；删掉窗口矩形回退（客户区空 ⇒ frame 被算成整个窗口大小，`want` 巨大，会按幻象几何 resize） |
| 3 | `platform::windows::set_window_region` | 拆出共享核心 `cut_region`；**严格化**——`GetClientRect` 读不到就不裁、返回 `(0,0)`，保留上一帧区域等下一帧 | 区域永远裁自**真实客户区**，不再有幻象矩形定心。`set_window_circle_region`（种子）独占保留 phantom 回退：没有"上一帧区域"要保，且第一帧方形比近似圆形更糟 |

**新增回归测试** `the_layout_box_is_the_rectangle_the_cut_is_taken_from`：把 `cut_region` 的直径算术（`client × region_d / win`，取整到设备 px）在纯函数层复现，对全部 usage × hover × morph 断言——布局框取本帧 `want` 时切出的直径恒等于请求值；同一算术换成陈旧的 360 框则**停靠态恰好腰斩**（145.6 → 59），即用户看到的碎片。这直接钉死"布局框必须等于裁剪所取的矩形"这条不变量，而不是再依赖一次根因猜测。

**验证**：`cargo fmt --check` 干净；`cargo check --all-targets` 零代码告警；`cargo test --lib` **196 passed**；`cargo build --bin tokenmonitor-app` 成功。真机验证需重启应用（旧二进制仍在跑，坏帧不会自愈）。

### 9.15 复发：窗口已回自由态、球也画在中心，但**区域还是停靠态那块**（2026-10-14 晚）

**现象**：从停靠方块拖出后，球只剩左上角一小块（用户第二次报告"还是有问题"）。与前三轮截图形状完全不同。

**取证（这次每一行都是量出来的，没有推断）**：

| 证据 | 数值 | 含义 |
|---|---|---|
| 截图尺寸 | 376×399 | **就是球窗口本身**（360 客户区 + (16,39) 边框）⇒ 截图那一刻窗口**已经是自由态 360** |
| 可见内容 bbox | x 102..154、y 125..177（53×53） | 只有球的一角 |
| 右边界 | x=154，**53 行全在 154，一条笔直竖线** | 硬边，不是圆弧 ⇒ 矩形裁剪 |
| 底边界 | y=177，直线 | 同上 |
| 球弧拟合 | 圆心 **(188, 210)**、r≈92，14 个采样点残差 0.6px | **球画在自由态中心**（客户区中心 180+8=188、180+31=211） |
| 实测停靠态 | client 148×148、region **143×143@(10,33)** | 裁剪矩形 = 停靠态那块，一模一样 |

⇒ 结论唯一：**窗口已经回自由态、球也正确画在中心，但 `SetWindowRgn` 的区域还是停靠态那块 ~144@(10,33)**，于是 OS 在一个 ~150px 的球上开了个 ~145px 的洞，只剩左上角。

**为什么没被重切**：`render` 的重裁判据是 `clip_client != last_clip_client || region_moved(...)`，而 `last_clip_client` / `last_region` 记的是**上一次请求**的裁剪，不是**实际在生效**的裁剪。三条路径都能让这个 memo 说谎：

1. `set_window_region` 内部**自己再测一次** `GetClientRect`——那是另一个瞬间。窗口在这个瞬间被改尺寸（`WM_DPICHANGED` 的建议 rect、排队中的 `SetWindowPos`、morph 的"冻结→落定"resize），切出来的区域就属于另一个矩形，而 memo 记的是外面那次测量的结果 ⇒ **永久说谎**，直到下一次几何真的变了才被发现。这就是本帧。
2. `set_window_region` 返回 `(0,0)`（区域对象创建失败）时，`last_clip_client` 被写成 `(0,0)`、`last_region` 被更新——**记录了一次根本没发生的裁剪**。
3. `last_region` 只含逻辑量，而区域的设备几何是客户区的**分数**；`WM_DPICHANGED` 改掉设备尺寸时逻辑量一动不动。

**修法（把"一帧只有一个矩形"变成结构性事实，而不是靠判据去猜）**：

| # | 位置 | 改动 |
|---|------|------|
| 1 | `fit_floating_card_window` | **返回它最终收敛到的客户区尺寸（设备 px）**，`Option`。调用方不再自己重测——第二次 `GetClientRect` 就是第二个瞬间 |
| 2 | `ui::floating::render` | 布局框、`set_floating_card_frac`、区域裁剪**全部读这同一个值**（`client_dev`）。删掉 `measured_client`（它就是那个"第二次测量"） |
| 3 | `set_window_region` | **签名收下调用方测好的客户区**，不再自己 `GetClientRect`；返回 `bool`（是否真的裁上了），不再返回"我用了哪个矩形" |
| 4 | `ui::floating::render` | **裁完、且 OS 复核窗口尺寸没变，才写 memo**；`last_region` 升级为 `RegionKey = (win, ratio, region_d, client_dev)`，设备矩形进 key；新增 `last_verified_client` |
| 5 | `ui::floating::render` | 客户区完全读不到时：不 resize、不裁切，布局用 `last_verified_client`——**这一帧没动过窗口，窗口就还是这个几何**。用 `want` 是错的：morph 半途取消时那个 ask 永远不会落地 |

**为什么 `last_verified_client` 不是上一轮那个"陈旧记忆"的同类**：走到这个分支的前提是**本帧没有对窗口做任何 SetWindowPos**，所以窗口保持的恰好就是上一次验证过的几何，布局框和裁剪矩形天然同源；而 `want` 描述的是一个**可能永远不会存在**的窗口。下一帧重新测量，球会自己长回/缩回正确大小——宁可小一帧，不可被切一角。

**验证**：`cargo fmt --check` 干净；`cargo check --all-targets` 零告警；`cargo test --lib` **196 passed**（`identical_regions_are_not_reapplied` 已扩到四元 key，覆盖"客户区变了但逻辑量没变"和"没有可测客户区"两种情况）；`cargo build --bin tokenmonitor-app` 成功。

### 9.16 复发：不是 memo，是 `SetWindowPos` 与 `SetWindowRgn` 之间的中间态（2026-10-14 夜）

**现象**：从屏幕边框拖到中间时出现那个碎片，再拖一下就好。

**这次日志直接抓到了现场**（`%TEMP%\tm-frame.on` 布着，用户复现一次，2481 行）：

```
751968671 frame  ... docked=true  client=153.0x153.0 region=(10, 33, 158, 181)
751968671 fit:   side=360.0 cur_window=(169,192) cur_client=(153,153) -> window=(376, 399)
751968687 msg=0x0047 client=(360, 360) region=(10, 33, 158, 181)   ← 碎片
751968687 msg=0x0003 client=(360, 360) region=(10, 33, 158, 181)
751968687 msg=0x0005 client=(360, 360) region=(10, 33, 158, 181)
751968687 msg=0x0046 client=(360, 360) region=(10, 33, 158, 181)
751968703 msg=0x0047 client=(360, 360) region=(72, 95, 303, 326)   ← 修好
751968703 frame  ... docked=false client=360.0x360.0 region=(72, 95, 303, 326)
```

**关键事实**：

1. `fit:` 行与随后的 `frame` 行**同一 tick（751968671）** ⇒ resize 与 cut 是背靠背的，中间的消息都是 `SetWindowPos` 内部同步发出的。**所以"两帧之间没重切"这个上一轮的假设不成立。**
2. 但 `WM_WINDOWPOSCHANGED`(0x0047) 那一刻的**窗口状态**是：窗口已经是 376×399 / 客户区 360×360，**区域仍是停靠态那块 (10,33)-(158,181)**。Windows 会把这个中间态呈现出来 —— 就是用户看到的那一块。
3. 全日志共 **39 处**同签名，全部集中在「窗口变大」的方向。「窗口变小」的方向无害：旧区域比新窗口大，Windows 求交后等于整个窗口，球完整。
4. 用户截图量到的裁剪区 x≤160 / y≤183，与日志的 region 右/下边界 158 / 181 差 2px —— 正是 Win32 region 右/下边界 exclusive 的量法差。**完全吻合。**

**真根因**：**Win32 region 活在窗口坐标系里，`SetWindowPos` 既不平移也不缩放它。** "先 resize 窗口、后重切区域"这个顺序本身就会产生一个「已放大的窗口 + 旧的小区域」的状态；这个状态无法靠判据避免，只能靠**顺序**避免。§9.15 修的是 memo（记录的东西对不对），这一轮暴露的是**顺序**（什么时候切）。

**修法：先切区域，再 resize。**

Win32 允许 region 比窗口大（超出的部分被窗口矩形求交掉）。所以：

1. 用**本帧要求的目标客户区** `want_client = round(want × scale)` 先切区域。此刻窗口还是小的，这个"未来的"区域与当前窗口求交 = **整个窗口 ⇒ 不裁剪**，球完整可见。
2. 再 `SetWindowPos` 把窗口长到目标尺寸 —— 此时区域已经描述了新窗口，中间态**不再产生洞**。
3. memo 的 key 记的是**请求**几何（`want` / ratio / `region_d` / `want_client`），所以 resize 之后本帧自己的 cut 就算"已完成"，不会重复切；若 resize 没落地，复核读回的客户区对不上就不写 memo，下一帧重来。

**为什么 key 必须记"请求"而不是"实测"**：cut 发生在 resize **之前**，若 key 记实测（resize 前的旧客户区），每一帧 resize 都会被判成"已经做过了"，反而跳过本帧该装的那次 cut —— 正好把修复本身废掉。

**附带影响（无害）**：resize 时区域先按目标尺寸切好，窗口还是旧尺寸 ⇒ 这一帧等于不裁剪，四角透明。因为窗口是 layered 透明 popup，看不到方形边框，只是"闪一下没有圆角裁剪"，远比"球缺一块"轻。

**验证**：`cargo fmt --check` 干净；`cargo check --all-targets` 零告警；`cargo test --lib` **196 passed**；`cargo build --bin tokenmonitor-app` 成功。

### 9.17 复发：顺序修法只修好了一半，「变小」方向被新造出同一个洞（2026-10-14 夜）

**现象**：§9.16 的「先切后 resize」上线后，**停靠落定**时又出现碎片（球心的位置变了：这次球心在 (188,211) = 旧视口 360 的中心，而不是停靠位）。

**日志再次直接抓到**：

```
755402671 frame  ... anim=true docked=true card=99.5 want=360  region=(69, 92, 306, 329)
755402687 msg=0x0046  window=(3648,404,4024,803) client=(360,360) region=(69, 92, 306, 329)
755402687 msg=0x0047  window=(3648,404,4024,803) client=(360,360) region=(10, 33, 162, 185)  ← ★ 碎片
755402703 fit: side=156.6 cur_window=(376,399) cur_client=(360,360) -> window=(173,196)
755402703 msg=0x0047  window=(3750,506,3923,702) client=(157,157) region=(10, 33, 162, 185)
```

**这是「窗口变小」（360 → 157）**。用户截图量到：球弧圆心 (188,211)、半径 93.1–94.1、68 行残差 0.97px ⇒ **球是按旧视口 360 画的**；裁剪区 x≤164 / y≤187 ⇒ 客户区 (0,0)-(156,156) ⇒ **正是预切好的那块 152**。

**根因：两个方向需要相反的顺序，顺序修法只能修一个。**

| 顺序 | 窗口变大（拖出起飞） | 窗口变小（停靠落定） |
|---|---|---|
| 先 cut → 后 resize | ✅ 未来的大区域落在小窗口上 ⇒ 求交=整窗 ⇒ 无洞 | ❌ 未来的小区域落在大窗口上 ⇒ 左上角一个 152 的洞 |
| 先 resize → 后 cut | ❌ 旧的小区域留在大窗口上（§9.16 的碎片） | ⚠️ 旧的大区域留在小窗口上，切掉一点，但球也在缩 |

**§9.16 判断「变小无害」的前提是旧顺序**（那时旧区域比新窗口大，求交后≈整窗）。改成新顺序后，那个前提对「变小」不成立了。**两个方向都被真实复现过。**

**修法：三步，把 resize 夹在中间。**

```
(1) open_window_region  —— 把区域放开成「旧客户区 ∪ 新客户区」
(2) fit_floating_card_window —— resize
(3) set_window_region   —— 切到真正的圆角矩形
```

新旧客户区**同起点**（原点在窗口内固定 (8,31)），所以并集 = **较大的那个**。而一个覆盖整个客户区的区域 = **不裁剪任何东西**（Windows 把超尺寸的区域与窗口矩形求交）。于是无论窗口往哪个方向走，中间态都是「窗口完全可见」，球永远完整。第 (3) 步在 resize **之后**才把真正的形状装回去。

代价：resize 的那一帧多一次 `SetWindowRgn`。**只在真正发生 resize 的帧付**（每条 morph 的起飞与落定各一次），静止帧与 hover 帧（只有形状变化、没有尺寸变化）不付。

**验证**：`cargo fmt --check` 干净；`cargo check --all-targets` 零告警；`cargo test --lib` **196 passed**；`cargo build --bin tokenmonitor-app` 成功。

### 9.18 复发：`open_window_region` 装的是「圆」，不是「矩形」（2026-10-16）

**现象**：§9.17 上线后仍露一块，用户描述为「露一个三角」。

**根因（代码级，可数值验证）**：§9.17 第 (1) 步「放开裁剪」的实现是错的：

```rust
// 旧实现
let side = cw.max(ch) as f32;
cut_region(hwnd, side, /* corner_ratio = */ 1.0, side, cw as f32, ch as f32)
```

`CreateRoundRectRgn(l, t, r, b, w, h)` 的 **corner 参数是"角椭圆的完整直径"**。当 `w = r−l` 且 `h = b−t` 时，四条角弧正好相接，**圆角矩形退化成内切椭圆**——正方形就是圆。所以这一步装上去的是：

```
region = CreateRoundRectRgn(8, 31, 368, 391, 360, 360)
       = 圆心 (188, 211)、半径 180 的圆
```

而 `cut_region` 的注释里其实已经写明了这个退化行为（"passing the rectangle's own side as the ellipse diameter makes the four corners meet and the region degenerates into exactly the ellipse"）——**但我把这个性质用在了需要"矩形"的地方。**

**它为什么会切到球**：停靠态的小客户区在窗口左上角 (8,31)-(165,188)，球心 (86.5,109.5)。这个圆**不以小客户区为中心**，而是以大客户区中心 (188,211) 为中心：

| 量 | 值 |
|---|---|
| 球心 → 圆心距离 | 143.5 px |
| 球半径（停车卡 99） | 49.5 px |
| 光晕 2σ 尾（0.085×99×2） | 16.8 px |
| 球最远点 → 圆心 | **209.9 px** |
| 圆半径 | **180.0 px** |
| ⇒ 被切掉 | **29.9 px** |

留下的形状 = 圆弧 ∩ 圆形区域 = **一段外弧 + 两条直线边**，视觉上正是「一个三角」。这就是「放开裁剪」那一步自己又切了一刀。

**修法**：`open_window_region` 改用 `CreateRectRgn`，直接铺满 `max(旧, 新)` 的整块客户区——只有矩形才真正"不裁剪"：

```rust
let (origin_x, origin_y) = client_origin_in_window(hwnd);
let hrgn = unsafe { CreateRectRgn(origin_x, origin_y, origin_x + cw, origin_y + ch) };
```

顺带把「窗口被 OS 在背后改了尺寸」这一情况也纳入包围：原来的判断只看 memo（区域内三件套 + client），OS 单方面改窗口时 memo 不变 ⇒ 不会放开 ⇒ resize 又穿过旧区域。现在并入 `resizing = cur_client != Some(want_client)`。

**教训（写进 MEMORY）**：`char corner_ratio = 1.0` 在 `cut_region` 里是"圆"；要"不裁剪"必须用 `CreateRectRgn`。**同一族 API 里，"最圆"和"完全不裁"是两个不同的极端，不能互相借用。**

**⚠️ 本轮无法编译验证**：本机 MSVC 工具链与 Windows SDK 已被卸载（`C:/Program Files (x86)\Windows Kits` 不存在、`BuildTools\VC` 为空、`vswhere` 无任何安装、全盘只剩 Git/VBA6/VS2010 的 `link.exe`），`cargo` 已抓到 Git 的 GNU `link.exe` 而链接失败。**修复已落码，但需要恢复工具链后才能构建与真机验证。**

### 9.19 工具链恢复后的复核、逐场景核算与落定重贴边（2026-10-19）

**工具链已恢复**（rustup 1.95.0 激活、Windows SDK 与 `E:\VSBuildTools` 均在），§9.18 的修复首次通过编译：`cargo check --all-targets` 零告警、`cargo test --lib` 196 passed。

#### 9.19.1 重新分析：可见形状是三方交集，三方活在三个时钟上

碎片 = 某一瞬间「**屏幕内容 P** ⊄（**区域 R** ∩ **客户区 C**）」恰好被 DWM 采样。三方各有各的时钟：

- resize / cut 是 render 中段的**同步系统调用**（微秒–毫秒级）；
- 内容 P 的呈现（present）在 GPUI 帧回调**末尾**，永远晚于本轮全部系统调用；
- DWM 按自己的节拍合成。

§9.12–§9.18 六轮修复把 **R 与 C** 的关系理顺了（memo 四元键、单次测量、open→resize→cut）；本轮把**每一个系统调用之后的中间态**逐场景核算了一遍：

| 场景 | resize 方向 | 旧内容 P 是否 ⊆ 新 C ∩ 新 R | 为什么 |
|---|---|---|---|
| dock 落定（360→157） | 缩小 | **✓** | 停靠锚让**新卡片外缘 = 旧卡片外缘**，而 morph 落定处新旧卡片尺寸几乎相等 ⇒ 两中心差 ≤2px；区域（153@新中心）⊇ 旧卡片+光晕（~135） |
| undock 起飞（157→360） | 放大 | **✓** | 放大方向交集只增不减；最终大圆（203）⊇ 旧小卡片 |
| undock 落定 | 无 resize | ✓ | no-op |
| hover 进/出（自由态） | 无 resize | ✓ | 窗口恒 360；区域按新卡片切，⊇ 旧卡片（±光晕边缘，视觉即收缩本身） |
| DPI 变化 | 双向 | ✓ | fit 锚保证卡片中心不动；窗口缩放后仍远大于卡片 |

**结论：§9.18 协议 + 锚定数学在所有已知场景下闭合**——「内容落后一拍」确实存在，但每个场景的锚定恰好让旧内容落在新交集内。碎片复发史的最后一个已知根因就是 §9.17 的 union-装圆（§9.18 已修，本轮起才算验证过）。

#### 9.19.2 仍然存在的确定性缺口：贴边 7px（§9.10 遗留）与落定后无自愈帧

- 松手帧 `dock_and_snap` 只能按**松手那一刻发布的 frac**（morph 前的大卡片）估算贴边位；落定卡片是它的 `DOCK_SIDE_RATIO`，实测间隙 ~7px 而非 `DOCK_GAP`=10px。
- 落定帧之后球静止、不再重渲染——若落定 resize 因任何原因没落地（DPI 竞态、消息合并），没有任何机制会再试一次。

**修法：落定重贴边（`pending_snap`）**——把「一次性 landing_shift」在 dock 方向换成「下一帧用真实几何重 snap」：

| # | 位置 | 改动 |
|---|---|---|
| 1 | `platform::snap_floating_window_to_edge`（新） | 位置 only 平移，使「**当前发布 frac** 描述的卡片」外缘落在 `DOCK_GAP`；与 `dock_and_snap` 共用新抽的 `dock_card_geometry`（一处数学，两处读者） |
| 2 | `advance_morph` dock 落定分支 | 不再记 `landing_shift`，改为 `pending_snap = Some(edge)` 并 `request_animation_frame()`——静止球没有下一帧，必须显式要一帧 |
| 3 | `advance_morph` undock 落定分支 | 保留 `landing_shift`（外扩漂移矫正，方向已知、量微小） |
| 4 | `render` 末尾 | `landing_shift` 之后执行 `pending_snap.take()`——本帧所有几何操作完成后才贴边 |
| 5 | `set_docked(None)` | 取消尚未执行的 `pending_snap`（防 undock 与重 snap 打架） |

**为什么下一帧重 snap 是安全的**：位置 only 移动下区域与内容**随窗口一起走**，与客户区的交集不变——任何瞬间都无中间态可被采样；且落定帧已发布落定 frac，重 snap 量到的是**真正在屏幕上的卡片**，贴边由构造精确（7px 缺口连同根因一起消失）。

**为什么重 snap 替代 landing_shift 视觉等价**：两者终态都是「卡片外缘 = work_area − DOCK_GAP」（`the_resnap_lands_the_settled_card_on_the_gap` 钉死）；差别只是从「落定帧猜」变成「下一帧量」，多一帧（16ms）不可感知。

#### 9.19.3 回归测试

- `every_syscall_of_the_dock_landing_keeps_the_screen_card_visible`：把 §9.19.1 的核算写成可执行形式——对全部球径 × hover × 动画尾帧（0.9/0.95/0.97/1.0），断言「上一帧的卡片+光晕」在落定帧**最严格的瞬间**（cut 之后）仍在客户区与最终区域之内。锚定数学一旦被改坏，这条先炸。
- `the_resnap_lands_the_settled_card_on_the_gap`：发布 frac → 重 snap 卡片 → 右/左两侧外缘精确落在 `DOCK_GAP`。

#### 9.19.4 验证与遗留

`cargo fmt --check` 干净；`cargo check --all-targets` 零告警；`cargo check --bins --no-default-features --features tui` 通过（TUI 不被殃及）；`cargo test --lib` **207 passed**。完整 `cargo build --bin tokenmonitor-app` 因运行中的旧实例占用 exe 暂未执行——关闭应用后构建即可。

**真机验证清单（待用户重启应用）**：拖到左/右缘松手 → 方形贴边间隙 ≈10px（不再是 7px）；来回拖出/拖入无碎片；跨 DPI 屏拖出无碎片；`%TEMP%\tm-frame.on` 取证日志新增 `snap=` 行与帧日志 `snap={:?}` 字段。

### 9.20 碎片第七次复发：日志取证终获真凶——「内容跟随窗口移动」（2026-10-19 深夜）

**现象**：§9.19 修复（pending_snap 重贴边）上线、用户真机复测（exe 17:19:44 构建，进程 17:19:46 启动，确认跑的是新代码），**碎片仍复现**——¼ 圆扇形残片（左上弧 + 右/下两条垂直直边），且位于屏幕中部（自由球区域，非停靠边）。

**取证**：`%TEMP%\tm-frame.on` 打开后抓到 1963 行帧级日志（17:35:39–19:42:08），与墙钟对齐后锁定现场（19:41:47.5 最后一次 dock 落定，截图 19:41:50）：

```
297304500  open（region→(8,31,368,391)）
297304500  fit 第一轮 SetWindowPos：窗口 360→154 完成   ← 屏幕内容还是动画尾帧
297304515  fit 第二轮收敛复核：又一次 SetWindowPos        ← 间隙被拉长到 ~31ms
297304531  cut 完成（region→(10,33,159,182)）
```

**根因（结构级，被此前全部七轮修复遗漏）**：**窗口的表面内容随窗口原点一起移动**。`fitted_origin` 的锚定数学是在**屏幕坐标系**里设计的——「平移窗口使新客户区的中心对准旧卡片的屏幕位置」——但平移根本不改变内容与客户区的**窗口坐标**相对几何：旧卡片永远停在旧 360 布局的中心 (188,211)，而新客户区只有 (8,31)-(162,185)。**从 resize 完成到 GPUI present 新布局的整个间隙里，旧内容被新客户区裁成 23×23 的左上角碎片**；fit 的第二轮收敛复核把间隙拉长到 ~31ms ≈ 2 个 DWM 合成周期，每次落定必被采样。§9.19 的逐场景核算犯的正是同一个坐标系错误（假设锚定让中心重合 ⇒ 旧内容在新交集内——屏幕系成立、窗口系不成立）。

**修法（布局中心走 / layout-centre walk）**：

| # | 位置 | 改动 |
|---|---|---|
| 1 | `ball_box_at`（新） | 布局盒显式中心版——所有层（球体/高光/文字/图标）经同一盒派生，跟随走 |
| 2 | render 布局 | 动画期间卡片**不再画在包络中心**，而是沿「旧形态客户区中心 → 新形态客户区中心」插值（`morph_center`）；两端恰为各形态静止中心，衔接无缝 |
| 3 | render 末尾 | 动画期间每帧**原点反向补偿**（position-only move，不可能产生裁剪中间态）：dock 逐帧瞄准贴边线（卡片沿边缩小，中心渐外移），undock 恒持松手屏位 |
| 4 | `resize_floating_window_plain`（新 platform 原语） | morph 起飞/落定的 resize **只缩放不平移**——旧内容已在新客户区中心，平移反而把它带走（正是碎片本身）；未落定的 settle 下一帧继续 plain 而非退回锚定 |
| 5 | 动画期间区域 | 恒为 **open 矩形**（走位后的卡片跑出包络圆——圆心装不下插值路径；memo 用哨兵 key (-1,-1) 避免每帧重开）；真形状只在落定帧切一次 |
| 6 | `advance_morph` 落定 | dock：置 `settle_plain`+`pending_snap`；undock：**无矫正**（走位协议下卡片屏位从未移动，`landing_shift`/`envelope_landing_shift` 及其漂移模型整体删除） |

**效果**：落定的两个间隙（resize→cut、cut→present）里，旧内容（走位尾帧卡片 @目标客户区中心）⊆ 新客户区 且 ⊆ 新区域——**每个中间态在构造上完整**，不再依赖采样概率。视觉上 dock = 球沿贴边线缩小、undock = 原地长大，均无屏位跳动；每帧一次 move-only `SetWindowPos`（便宜，非 §9.11 的 resize+region 组合）。

**回归测试**（删 2 条被证伪的旧测试，新增 4 条）：`the_morph_walks_the_layout_centre_between_the_two_client_centres`、`every_walked_frame_keeps_the_card_inside_the_envelope_client`、`the_settle_shrink_leaves_the_walked_card_inside_the_new_client`（碎片回归的可执行形式）、`the_undock_takeoff_keeps_the_docked_card_inside_the_envelope`。

**验证**：`cargo fmt --check` 干净；`cargo check --all-targets` 零告警；`cargo test --lib` **209 passed**。真机复测：重启后反复 dock/undock；取证日志 frame 行新增 `center=` 字段，落定序列应见 `plain: side=153.9`（不再有 `fit:` 的双轮 SetWindowPos）。

### 9.21 第八次：dock 已闭合，undock 走位方向反了（2026-10-19 20:07 取证）

**现象**：§9.20 走位协议上线后用户复测仍"不行"。取证日志（4364 行，含新 `center=` 字段）显示：

- **dock 方向已完全闭合**：走位 `center` 81.5→78.8→78.0 无缝衔接落定；`plain` resize 原点不动（窗口 (1220,302) 保持）；cut 区域 (10,33,161,184) 完全居中包裹卡片（余量 14px/边）；`pending_snap` 次帧 1px 微调——**大小余量充足，排除"大小导致"**。
- **undock 方向有硬 bug**：`298700921 morph=1.000 anim=true center=180.0`——起飞帧卡片真实位置在停靠中心 (78,78)，插值却给出 (180,180)。

**根因**：走位插值参数直接用了 `morph` 值。dock（morph 0→1）时二者恰好重合（方向一致的巧合）；**undock（morph 1→0）时 morph=1 是起点、morph=0 是终点，与插值需要的 0→1 进度正好颠倒**——起飞帧把卡片画到走位远端，屏幕锚补偿把窗口原点拖移 102px；动画全程插值反向回走；落定帧 rest 布局又跳回。**两次 102px 级跳变**，undock 视觉闪跳。

**修法**：

1. **进度归一化**（`morph_walk_progress`）：`progress = (morph - morph_from) / (morph_target - morph_from)`，夹取 [0,1]——方向无关。dock/undock 的起飞帧 progress=0（卡片在实际位置），落定帧 progress=1（=rest 中心），两端均无缝。
2. **中途反向续走**（`anim_center_now`）：每帧记录走位当前中心；morph 在动画中被反向（快速 dock↔undock）时新走位从**实际位置**续走而非从形态中心重启（否则又一次瞬移）。屏位锚在补偿下恒定，反向时保持原锚即可。
3. **undock 落定也置 `settle_plain`**：两个方向的落定 resize 都保证 plain（原点不动），杜绝任何路径的锚定移动残留。
4. **dock→dock 换边补 snap**：停靠态拖到另一侧松手不触发 morph，此前永远不换边——`set_docked` 检测同形态换边时直接 `pending_snap = 新边`。

**回归测试**：新增 `the_walk_progress_is_direction_agnostic`（undock 起点必须=0/终点=1、单调、退化情形），改写 `the_morph_walks_the_layout_centre_between_the_two_client_centres`（undock 端点互换镜像断言）。

**验证**：`cargo fmt --check` 干净；`cargo check --all-targets` 零告警；`cargo test --lib` **210 passed**。

### 9.22 第九次：碎片是"停靠区域 + 自由内容"的僵尸窗口——区域从不自愈（2026-10-19 20:24 取证）

**现象**：§9.21 修复后用户复测仍见 ¼ 扇形残片（截图 `截图_20261019_202417_592.png`）。**截图尺寸 376×399 = 悬浮窗整窗尺寸（客户区 360 + 边框 16×39），即一张对悬浮窗本身的窗口截图**。

**像素级取证**（连通域分析）：主簇 bbox x[113..164] y[135..187]，52×53；弧半径反解 = 80px = 悬停球 160.4/2；球心反解 = (187.6, 211.1) = 自由形态客户区中心 (8+180, 31+180)；簇的右/下直边 = x≈163/y≈186 ≈ 停靠落定区域 (10,33,162,185) 的右/下边。**结论：截到的窗口处于「内容=自由+悬停球画在自由中心、区域=停靠落定的旧切口」的三方失步态。**

**日志交叉验证**（新进程 20:23:29 启动，era 1268 行）：
- 当前进程自己的窗口全程正确：dock-Left 走位 79→180 无缝、plain resize 原点不动、落定/悬停 cut 均被 msg 钩子与 frame 行双重观测到位（最后状态 region=(69,92,307,330)@20:24:04）；
- 20:24:04→20:24:17（截图时刻）之间**零事件**（无 frame、无 msg）——当前窗口不可能自己变成截图那样；
- 单实例互斥锁有效（fresh-start 序列出现 ⇒ 启动时无其他实例持锁）；
- 子类钩子只读不动区域、`region_box` 直读 `GetWindowRgn`（OS 真相，非缓存）。

**根因（缺陷类，与具体产生路径解耦）**：代码对区域的全部信任都来自 **memo**——memo 说"已安装"就永远不再核对。任何让实际区域与 memo/几何失步的路径（被中断的括弧、外部 `SetWindowRgn`、窗口重建残留、截图时刻仍在屏上的失步残留窗口）都没有任何检测与自愈手段，失步态**永久冻结**。这正是历次"修了又复发"的共同结构：每轮修的是产生路径，没人盯"已经坏了的窗口能否被发现并复原"。

**修法（防御性自愈，与产生路径无关）**：

| # | 位置 | 改动 |
|---|---|---|
| 1 | `expected_region_box`（新纯函数） | 一帧所需的区域盒：动画期=开区（客户区原点起的整矩形）、静止=居中 cut 盒。`region_box_matches` 带 ±3px 容差（两条舍入路径在任何 DPI 下差 ≤1px，不误伤健康区域） |
| 2 | render（truth-check） | memo 判定 `changing=false` 时，**直读 OS 真相 `region_box` 与期望盒比对**，失配 ⇒ 强制走 open→resize→cut 括弧——**失步窗口在下一帧渲染即自愈**，无论它由哪条路径产生 |
| 3 | `ensure_floating_window` 竞态防护 | hwnd 全局只在 open 回调里写入：回调注册前到达的第二次 ensure 会开出**第二个悬浮窗**（下层孤儿永不重渲染、区域冻结）。in-flight AtomicBool 使之成为 no-op |

**回归测试**：`the_region_truth_check_accepts_healthy_and_rejects_desynced_boxes`——健康 cut 盒（含分数 DPI 两舍入路径）必须通过、**停靠切口叠在 360 窗口上（本次残片的精确形态）必须被拒绝**。

**验证**：`cargo fmt --check` 干净；`cargo check --all-targets` 零告警；`cargo test --lib` **211 passed**。

> AI生成

### 9.23 第十次：自愈只活在帧里——静止窗口永不复核（2026-10-19）

**现象**：§9.22 真值核对上线后复测残片依旧。且复测时 `%TEMP%\tm-frame.on` 不存在——**取证全程没开，frame 日志零行**，残片发生时的几何未留下任何记录。

**结构性根因**：§9.22 的区域真值核对写在 `render` 里，而静止球**不出帧**——morph 落定后 `request_animation_frame` 链条终止，唯一的唤醒是 hover/拖拽事件。残片一旦在静止态出现（或落定后残留），没有「下一帧」，heal 永远不会执行：检测器存在，但它的心跳停止了。

**修法（独立心跳）**：

| # | 位置 | 改动 |
|---|---|---|
| 1 | `FloatingView.region_truth`（新字段） | 最后一个**静止** render 验证过的期望区域盒；动画期/待重切帧为 `None`（此时 render 侧检查自己管区域） |
| 2 | render 真值核对 | 健康且静止 ⇒ 发布 `region_truth = Some(expected)`；失配进入括弧或动画期 ⇒ 清空 |
| 3 | 区域哨兵（`new()` 里 spawn） | 800 ms 后台循环：窗口存活且 `region_truth` 在位时直读 `GetWindowRgn`，与期望盒失配 ⇒ `frame_log` 记录 actual/expected 并 `cx.notify()` 唤醒一帧——render 侧检查随即强制 open→resize→cut，失步窗口 ≤0.8 s 自愈，**与是否出帧无关** |

**取证配套**：`tm-frame.on` 已重建，下次复现时 `%TEMP%\tm-frame.log` 会留下 `sentinel: region desync actual=… expected=…` 行，可直接判定失步窗口与产生时刻。

**验证**：`cargo fmt --check` 干净；`cargo check --all-targets` 零告警；`cargo test --lib` **211 passed**。

> AI生成

### 9.24 第十一次：区域被整只摘除，而哨兵恰好看不见"无区域"（2026-10-19 21:20 取证）

**现象**：§9.23 哨兵上线后复测残片依旧。`tm-frame.log` 978 行、**0 条 sentinel 失步**——本进程窗口在帧层面全程健康。

**外部探针（关键证据）**：残片在屏时用 ctypes `EnumWindows` + `GetWindowRgn` 直读 tokenmonitor-app 进程的全部顶层窗口：

```text
hwnd=0x606ea Zed::Window rect=(-78,117,298,516)  rgn_hr=0  rgn=(0,0,0,0)   ← 悬浮窗
hwnd=0x1506e4 Zed::Window rect=(125,21,1241,748) rgn_hr=0  rgn=(0,0,0,0)   ← 主窗
```

悬浮窗 rect 与 frame 日志最后一帧完全一致，但 **`GetWindowRgn` 返回 0（ERROR）——窗口区域被整只摘除**，而日志末帧它还是健康安装的 (73,96,302,325)。即：日志之后有某条外部路径（GPUI 重建/OS 剥除，产生路径待 sentinel 行定位）把区域清空，窗口静止无帧，render 检查永远不会跑。

**结构性缺口**：§9.23 哨兵写了 `let Some(actual) = region_box(hwnd) else { return }`——**`None`（无区域）恰好被跳过**，唯一最严重的失步形态成了哨兵的盲区；render 侧检查虽把 `None` 判失步，但静止时无帧可跑。

**修法**：

1. 哨兵改为 `!actual.is_some_and(|a| region_box_matches(a, expected))`——`None` 也是失步，唤醒 render 强制重切；
2. render 真值核对在 `None` 分支单独记 `truth-check: region GONE` 行，下次复现可直接定位区域被摘除的时刻。

**验证**：`cargo fmt --check` 干净；`cargo check --all-targets` 零告警；`cargo test --lib` **211 passed**。

> AI生成

### 9.25 第十二次：区域健康而内容过期——遮挡吃掉了落定帧（2026-10-19 21:5x 取证）

**现象**：§9.24 构建下复测，残片（¼ 弧叠在主窗热力图/编辑器上）依旧，且同一时刻**真球完好**（两张截图并存的用户取证）。日志 5425 行：仅 1 条 `floating-start hwnd=0x2c02d8`（无窗口重建），**0 条 sentinel / region GONE**——整个会话区域从未失步。

**结论修正**：本轮残片不是区域失步，而是**表面内容过期**。区域与 memo 一致（哨兵判定健康），但窗口 redirection surface 上还是旧像素：球画在旧形态中心，切口是当前形态的偏心盒，交集即 ¼ 弧。产生机制：morph/拖拽期间窗口被主窗完全遮挡 → GPUI/DWM 停止上屏 → 落定帧（含 settle 重切）未呈现 → 无新帧刷新，过期像素永驻。

**修法（心跳从「事件驱动」升级为「恒定」）**：哨兵在静止期（`region_truth` 在位）**无条件 `cx.notify()`**——区域失步仍记日志，但唤醒不再依赖失步：过期表面 ≤0.8 s 必被重绘。静止球 1.25 fps 的渲染成本可忽略。

**验证**：`cargo fmt --check` 干净；`cargo check --all-targets` 零告警；`cargo test --lib` **211 passed**。

> AI生成

### 9.26 第十三次：区域从未失步——「摘除」是跨进程探针的伪证；嫌疑转向局部呈现（2026-10-19 22:0x 取证）

**现象**：§9.25 心跳构建下残片依旧，>1 s 不消失。日志 7153 行：2 条 `floating-start`（两次启动，无窗口重建），**仍 0 条 sentinel / region GONE**；末尾静止帧每 ~800 ms 一帧（心跳生效），逐帧 `region=Some((73,96,302,325))` 健康居中、球心 (180,180) 同心。

**证据修正**：跨进程探针两次读到 `GetWindowRgn=ERROR`，但应用内每帧直读同窗口区域均健康（frame 行 `region=` 即 `region_box` 的 OS 真值）——**跨进程 `GetWindowRgn` 对该窗口不可信，§9.24「区域被整只摘除」系探针伪证，作废**。区域自始至终健康。

**重新归因**：残片 = **呈现层（present）问题**。布局、区域、内容在应用侧全部正确，但上屏的合成结果停在一张局部帧——¼ 弧 = 一次 damage sub-rect 呈现只有四分之一被合成，其余滞留旧帧；静止球无自发帧，局部呈现永不修复。探针同时发现系统运行着 **AweSun 远程控制**（`OrayUI_Shadow` 窗口）——远程捕获链路是干预 DWM 呈现/合成的头号嫌疑，残片也可能只存在于远程视图。

**修法（全量 damage 防御）**：新增 `platform::invalidate_whole_window`（user32 `InvalidateRect(hwnd, NULL, FALSE)`，手写 extern 风格）；哨兵心跳每 800 ms 唤醒前先**全窗失效**——下一次 present 的 damage 被迫覆盖整窗，局部呈现无从立足。

**验证**：`cargo fmt --check` 干净；`cargo check --all-targets` 零告警；`cargo test --lib` **211 passed**。

> AI生成

### 9.27 第十四次：undock 落定的 SetWindowRgn 丢了 DWM 重合成——新内容一直被旧切口裁着（2026-10-20 取证）

**用户复现描述（决定性）**：圆形居中 → 拖到边框变方形（dock）→ 再拖回屏幕中间（undock）→ **确定性出现扇形残片**；「稍微动一下屏幕」即复原。

**日志闭环**（§9.26 心跳构建）：undock 落定帧（304946562）区域切到居中 (73,96,302,325)，其后 **20 秒静止心跳帧全部正确**——应用侧每 800ms 绘制并 present 的内容、读到的区域、memo 三方一致；直到 21.2s 后 hover 触发括弧重新 SetWindowRgn，残片才消失（「动一下就复原」的机制）。期间无 sentinel / region GONE。

**根因（确定）**：undock 落定帧的 `SetWindowRgn(bRedraw=1)` **丢失了 DWM 重合成**——区域对象已换新，合成器仍持旧停靠切口裁剪此后的所有 present。残片 = 正确新内容 ∩ 旧停靠形状 = 停靠球轮廓的右下四分之一；心慌的「局部呈现/内容过期」假设（§9.26）被日志否定：present 一直在发生，坏的只是合成器侧的形状。

**修法（把形状同步焊进调用链）**：新增 `platform::nudge_region_composition`——零位移 `SetWindowPos(SWP_NOMOVE|NOSIZE|NOZORDER|NOACTIVATE|FRAMECHANGED)` 强制帧路径重跑、DWM 重新拾取窗口形状；`cut_region` 与 `open_window_region` 每次 `SetWindowRgn` 成功后立即微推（`seed_window_region` 经 `set_window_circle_region` → `cut_region` 自动覆盖）。漏掉一次重合成也无法活过一帧。

**验证**：`cargo fmt --check` 干净；`cargo check --all-targets` 零告警；`cargo test --lib` **211 passed**。

> AI生成

### 9.28 第十五次：nudge 时机错误——render 中段微推把括弧瞬态冻上了屏（2026-10-20 取证）

**现象**：§9.27 的 render 内微推上线后，残片变成了**整窗空白**：黑块 ≈ 悬浮窗整窗（376px），无区域裁剪、内容全黑；且球被拖到了**第二台显示器**（日志 x=3267、探针 x=3808，跨 DPI 96）。

**根因（时机）**：nudge 本身方向正确（hover 复原 = 静止态重合成的偶然验证），但放在 render 内部 = 括弧（open→resize→cut）的**中段**——DWM 被强制重合成时采样到的是「区域全开 + resize 后未呈现的黑背板」这一瞬态，并把坏状态冻上屏：整窗空白。nudge 在 render 里每次 SetWindowRgn 后都触发，瞬态暴露面最大。

**修正（挪到静止态心跳）**：`nudge_dwm_composition`（原 `nudge_region_composition` 改名）从 `cut_region` / `open_window_region` 内部**移除**，改由哨兵在静止态调用——此刻内容、区域、memo 三方已验证一致，重合成采样到的必然是最终正确状态，无论修「形状过期」（FRAMECHANGED 让 DWM 重读区域）还是「表面过期」（InvalidateRect 全窗失效）都只会捕捉到好状态。文档明确：**只许静止态调用**。

**验证**：`cargo fmt --check` 干净；`cargo check --all-targets` 零告警；`cargo test --lib` **211 passed**。

> AI生成

### 9.29 第十六次：整个 `SetWindowRgn` 机制退场——形状交给绘制，命中交给 HTTEST（2026-10-20）

**现象**：§9.28 心跳 nudge 构建下，残片与空白**都还出现**，且动一下屏幕就复原。至此确认：只要窗口带 region，「合成器侧形状/内容过期」这一类问题就修不完——nudge 挪到哪里都只是在猜 DWM 什么时候会丢、丢哪一帧。

**决策（根修）**：**`SetWindowRgn` 机制整体退役。** 悬浮窗回归普通透明无边框矩形窗口，球的圆形由 GPUI 绘制本身呈现（本就如此），圆外的点击穿透由子类 `WM_NCHITTEST` 自己回答——形状不再有「应用侧状态」与「合成器侧状态」两份，过期失步失去存在基础。

**floating.rs**：

- 删除 render 的 open→resize→cut 括弧、区域 memo（`last_region`/`last_clip_client`/`RegionKey`）、真值核对（`expected_region_box`/`region_box_matches`）、`region_truth` 字段与哨兵失步分支；render 只剩一步 plain/anchored resize。
- 删除 `seed_window_region`、`SEED_CLIP_FRACTION`、`CLIP_MARGIN` 及其常量注释；`clip_diameter_for` 语义改为「卡的适配尺寸」（docked 窗口边长），不再切割任何东西。
- 哨兵简化为纯呈现心跳：`InvalidateRect` 全窗失效 + `cx.notify()`（nudge 一并移除——没有 region 后 DWM 不再需要被提醒形状）。
- 测试增删：删 seed/真值核对/memo 键/镜像 cut 算术四类测试；`window_wraps_the_card_and_its_glow`、`the_free_ball_keeps_the_fixed_window`、envelope/pinning/settle/包含测试全部改写为窗口几何语义（`the_window_contains_the_card_and_its_glow` 替代 region 版）。

**platform/windows**：

- 删除 `set_window_region`、`set_window_circle_region`、`open_window_region`、`cut_region`、`region_box`、`nudge_dwm_composition` 与 gdi32 区块（`CreateRoundRectRgn`/`CreateRectRgn`/`GetWindowRgn`/`GetRgnBox`/`DeleteObject`）及 `SetWindowRgn` extern、`SWP_FRAMECHANGED`；`platform/mod.rs` 同步收窄导出。
- `ball_drag_subclass` 的 `WM_NCHITTEST` 从无条件 `HTCAPTION` 改为 **`ball_hit_test`**：把屏幕坐标映射进逻辑客户区，与「卡半径（`FLOATING_CARD_FRAC`）+ `HIT_SLACK`(12px) 光晕余量」的圆做距离测试——圆内 `HTCAPTION`（拖拽路径），圆外 `HTTRANSPARENT`（穿透桌面，360/Thunder 观感）。几何不可信的首几帧回退全窗 `HTCAPTION`，不吞按键。
- msg/frame 日志行去掉 region 字段；受影响文档（`client_size_logical`/`client_size_device`/`client_origin_in_window`/拖拽子类 1.）全部改写为无区域语义。

**验证**：`cargo fmt --check` 干净；`cargo check --all-targets` 零告警；`cargo test --lib` **207 passed**（-4：删除的是区域机制自身的测试）；`cargo check --bins --no-default-features --features tui` 通过。

**遗留语义变化**：圆外的光晕不再被硬切割（此前区域落在光晕已淡出的空环上，视觉无差）；球的抗锯齿由 GPUI 渲染负责，边缘反而比 region 的硬切更平滑。风险点：`ball_hit_test` 在拖拽中窗口尺寸变化时的分数换算依赖 render 持续发布 `card_frac`——静止时不发布新值，沿用上次的即正确。

> AI生成
