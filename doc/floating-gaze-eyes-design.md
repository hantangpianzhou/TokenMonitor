# 悬浮球「眼睛跟随」交互设计说明

> 状态：**设计中（未实现）**。
>
> 参考对象：某悬浮小部件（两只「眼睛」的瞳孔跟随鼠标指针方向转动）。用户提出将这一指针跟随交互借鉴到 TokenMonitor 悬浮球（`src/ui/floating.rs`）。
>
> 已确认的取舍：
> - **方案**：A —— 复用现有刷新按钮位，将单枚「↻」改为两只并排眼睛（不新增元素、不挤占数字布局）。
> - **跟随范围**：仅悬浮球内跟随 —— 鼠标在球内时瞳孔跟随方向，移出后瞳孔回中；不做全屏跟随。

---

## 1. 动机与目标

悬浮球是常驻置顶的小部件，用户会长时间与它对视。「眼睛跟随鼠标」是零成本的趣味交互：

- 让悬浮球「活」起来，从静态数据牌变为有生命感的小部件；
- 不新增控件、不挤占球内数字/标签空间，只改造现有刷新按钮的视觉；
- 刷新功能完全保留，交互语义不因外观变化而丢失。

**明确不做**：

- 不做全屏跟随（鼠标离球后无意义乱看，分散注意力）；
- 不做桌面宠物式多元素（只用一只按钮位，保持悬浮球克制）；
- 首版不加眨眼/表情（见 §5 待定项）。

## 2. 交互设计

### 2.1 位置与形态

现有刷新按钮位于球底部中央（`ICON_CY_RATIO`，逻辑 px 命中区由
`set_floating_refresh_hit` 发布）。改造后该位置为**两只并排的眼睛**：

```
        ┌─────────────────┐
        │   123,456,789    │   ← 数字（不变）
        │     今日         │   ← 周期标签（不变）
        │    ◉   ◉         │   ← 双眼（原 ↻ 位）
        └─────────────────┘
```

- 每只眼睛 = 白色半透明眼球（圆） + 深色瞳孔（圆）；
- 双眼总宽 ≈ 现有 `icon_d`（22px，随 `card_d * 0.22` 缩放），单眼约 9px；
- 两只眼睛中心间距 ≈ 4px（逻辑 px）。

### 2.2 状态机

| 状态 | 触发 | 表现 |
|------|------|------|
| 静止 | 鼠标不在球内 | 瞳孔居中（正视前方），眼球为现有按钮底色（白色 18% 透明 + 细边） |
| 跟随 | 鼠标在球内 | 瞳孔朝鼠标方向偏移，最大 3px（钳在眼球内） |
| 离开 | 鼠标移出球 | 瞳孔平滑回中（120ms 线性缓动） |
| 刷新中 | `refreshing == true` | 双眼隐藏，显示现有旋转 `LoaderCircle`（复用现有动画路径） |
| 点击 | 命中区按下 | 仍是「刷新」，走现有 `TrayCommand::Refresh` 路由 |

### 2.3 命中区与可访问性

- 命中区**不变**：两眼整体矩形 = 现有 `set_floating_refresh_hit` 发布的矩形，Win32
  拖拽子类（`ball_drag_subclass`）零改动；
- 眼睛不是纯装饰：它仍是刷新按钮，hover 时眼球略亮（沿用现有
  `hovered ? 0.34 : 0.18` 透明度逻辑），保证「可点击」的视觉提示。

## 3. 技术设计

### 3.1 数据来源（全部已有平台能力，新增一个公开封装）

| 需要的数据 | 现有能力 | 改动 |
|-----------|---------|------|
| 全局鼠标坐标 | `GetCursorPos`（`platform/windows/mod.rs` 已声明） | 新增 `pub fn cursor_screen_pos() -> Option<(i32, i32)>` |
| 窗口屏幕矩形（设备px） | `window_rect(hwnd)` | 无 |
| 客户区在窗口内偏移 | `client_origin_in_window(hwnd)` | 无 |
| DPI 缩放 | `window_scale(hwnd)` | 无 |
| 球心（逻辑px） | 现有 `card_left + card_d/2`、`card_top + card_d/2` | 无 |

### 3.2 每帧计算（render 内，纯函数）

```
cursor_screen = cursor_screen_pos()                        // 设备 px
rect          = window_rect(hwnd)                          // 设备 px
scale         = window_scale(hwnd)
origin        = client_origin_in_window(hwnd)              // 设备 px
cursor_logical = ((cursor_screen - rect.左上) - origin) / scale
dx = cursor_logical.x - ball_cx
dy = cursor_logical.y - ball_cy
dist = hypot(dx, dy)
in_ball = dist <= ball_radius + 2.0                        // 含 2px 容差
offset = if in_ball {
    (dx / dist * MAX_OFFSET, dy / dist * MAX_OFFSET)       // MAX_OFFSET = 3.0
} else {
    (0.0, 0.0)                                             // 或向 0 缓动
}
```

- 纯函数签名：`fn gaze_offset(pointer: (f32, f32), center: (f32, f32), radius: f32) -> (f32, f32)`，便于单测。
- 缓动回中：首版不做插值状态，直接归零即可（3px 的位移无跳变感）；若效果生硬，再加 120ms 线性缓动（记录 `Instant`，与现有 `morph_started` 同风格）。

### 3.3 渲染（`render` 内替换 §1.5 刷新图标）

- 眼球：`div().rounded_full().bg(WHITE.opacity(0.90))`，尺寸 `icon_d * 0.55`；
- 瞳孔：`div().rounded_full().bg(主题深色.opacity(0.55))`，直径 `icon_d * 0.30`；
  位置 = 眼球中心 + `gaze_offset` 结果；
- 背景环：保留现有按钮底色圆（`bg(WHITE.opacity(if hovered {0.34} else {0.18}))` +
  1px 白边），两只眼睛画在其内；
- `refreshing` 分支：**原样保留** `LoaderCircle + with_animation` 路径，仅非刷新态换为双眼。

### 3.4 命中区

- 两眼整体仍是 `icon_d × icon_d` 的按钮矩形，`set_floating_refresh_hit` 输出不变；
- Win32 侧（`mod.rs` 中 refresh-hit 判定）无需任何修改。

## 4. 边界与风险

| 风险 | 对策 |
|------|------|
| 鼠标在球外瞳孔乱动 | `in_ball` 判定加 2px 容差，球外恒为 (0,0) |
| 与拖动冲突 | 拖动走 Win32 原生路径（`WM_NCHITTEST`/`HTCAPTION`），GPUI 侧只读指针、不拦截事件，无冲突 |
| 与刷新点击冲突 | 命中区不变，点击路由不变 |
| docked 方形形态 | 眼睛几何随 `card_d` 缩放（同现有图标 `icon_d`），无需新分支 |
| 0 用量最小球（`MIN_SPHERE`） | 22px 图标位放两只 9px 眼睛可行；需验证 8 位数字最宽时眼睛不被数字覆盖（现有 `overlay_shift_up` 已保证间距） |
| 双屏/高 DPI | 全程设备 px → 逻辑 px 换算，与现有布局同一套口径 |

## 5. 待定项（实现时拍板）

1. **瞳孔颜色**：主题色（跟随主题） vs 纯黑 `rgba(0,0,0,0.55)`（更"眼睛"）。倾向主题色，与悬浮球整体一致。
2. **回中缓动**：直接归零 vs 120ms 线性。首版建议直接归零，观察再定。
3. **眨眼**（800ms 周期 1 帧闭眼）：首版**不做**，保持克制；后续可作为可选项。
4. **眼睛间距/大小微调**：依赖实际渲染观感，与用户现场确认。

## 6. 实施步骤（预估 0.5~1 天）

1. `src/platform/windows/mod.rs`：新增 `cursor_screen_pos()` 公开封装（含 hwnd==0 兜底），导出至 `platform/mod.rs`。
2. `src/ui/floating.rs`：新增 `gaze_offset` 纯函数 + 单元测试（球内 4 方向、边界容差、球外归零）。
3. `src/ui/floating.rs`：render 中替换刷新按钮为双眼，接入偏移；`refreshing` 分支保留原 `LoaderCircle` 动画。
4. 构建 + 全量测试（`cargo test`，悬浮窗几何不变量：`overlay_shift_up`、`label+count` 拟合、`speculars_stay_clear` 均不受影响）。
5. 用户侧手动验证：自由形态跟随、点击刷新、刷新转圈、dock 方形形态、0 用量最小球、缩放显示器。

## 7. 验收标准

- [ ] 鼠标在球内：瞳孔始终指向鼠标方向，最大位移 3px，且瞳孔不溢出眼球；
- [ ] 鼠标移出：瞳孔回中，无残留偏移；
- [ ] 点击双眼 = 刷新：与现有刷新按钮行为一致（含双击开窗、拖拽不误触）；
- [ ] 刷新进行中：显示旋转 LoaderCircle（非双眼）；
- [ ] docked 方形与自由圆形两种形态下几何正确；
- [ ] 现有测试全部通过，无新增回归。

> AI生成
