# 悬浮球桌宠化（v1「活球」）：设计与实施规格

> 状态：**已移除（2026-10-21）**。v1 曾完整实现并通过 211 项测试，但用户实际试用后判定"功能不好用"，当日完整移除：眼睛跟随/眨眼/呼吸/拖拽表情全部删除，刷新按钮还原为 ↻ 图标，`FloatingView` 与平台层回到纯球形态（203 项测试）。本文档保留作为历史记录——若未来重做，§2 的参数与 §6 的"呼吸只进绘制层、永不进布局层"分层原则（首版文字跟着球动的根因与修正）仍适用。
>
> 定位：TokenMonitor 的桌宠不是纯摆件，而是**用量的人格化仪表盘**——角色行为由真实数据驱动。本文档是方案 A（v1）的实施规格；方案 B（序列帧角色）/ C（完全体桌宠）见 §10 展望。
>
> 前置文档：`floating-gaze-eyes-design.md`（眼睛跟随的最初设计，本文档是其超集并拍板了全部待定项）。

---

## 1. 分档与 v1 选型

| | A. 球「活」起来（**v1，本文档**） | B. 序列帧角色 | C. 完全体桌宠 |
|---|---|---|---|
| 形象 | 保留现有球体 | 独立角色 sprite | 角色 + 气泡 + 走动 |
| 动画 | 程序化：眼睛跟随、眨眼、呼吸、拖拽表情 | 多帧序列帧 | + 随机行为、对话 |
| 素材 | **零依赖，纯代码** | 需 sprite 素材 | 素材 + 状态机 |
| 风险 | 极低（不动呈现管线） | 中 | 高（持续重绘功耗账） |

**v1 范围**（四个「活」的细节，全部纯代码）：

1. **眼睛跟随**：刷新按钮位改为两只眼睛，瞳孔朝球内鼠标方向偏移（`floating-gaze-eyes-design.md` 方案 A）
2. **眨眼**：3.5~6.5s 随机间隔，一次 320ms（闭 140 / 保持 60 / 睁 120）
3. **呼吸**：球径 ±1.5% 的 4s 正弦缩放
4. **表情联动**：拖拽中瞳孔收缩（「紧张」）；刷新中双眼退位给现有 LoaderCircle 旋转

**明确不做（v1）**：sprite 素材、气泡对话、自动走动、眨眼以外的表情、桌宠开关设置项（v2 候选，见 §10）。

## 2. 状态与参数（全部定死）

| 参数 | 值 | 说明 |
|---|---|---|
| `BREATH_AMP` | 0.015 | 呼吸幅度 ±1.5%；hover pop 是 6% 的既有先例，1.5% 远在其内，fit 无虞（§6） |
| `BREATH_PERIOD` | 4.0 s | 成年人安静呼吸约 12~16 次/分，4s 周期最自然 |
| `BLINK_CLOSE/HOLD/OPEN` | 140/60/120 ms | 合计 320ms；人类眨眼 100~400ms |
| `BLINK_MIN_GAP / MAX_GAP` | 3.5 s / 6.5 s | 随机间隔上下界（splitmix64 伪随机，确定性但无规律感） |
| `TICK_BLINK` | 40 ms | 眨眼进行中的 tick（25 fps，眼睑动画平滑的下限） |
| `TICK_HOVER` | 90 ms | hover 中（瞳孔跟随的响应粒度，≈11 fps） |
| `TICK_IDLE` | 150 ms | 静止呼吸采样（4s 周期 ≈ 27 采样/周期，肉眼平滑） |
| `TICK_HIDDEN` | 800 ms | 窗口隐藏时（与哨兵同拍，且 **不发 notify**） |
| `GAZE_TOL` | 2.0 px | 球外容差带：出球 2px 内仍跟随（避免边缘跳变），再外归零 |
| `EYE_PAD / EYE_GAP` | 2.5 / 3.5 px | 双眼在按钮圆盘内的边距与间距 |
| `EYE_PUPIL_RATIO` | 0.5 | 瞳径 = 眼径 × 0.5 |
| `EYE_SHIFT_HEADROOM` | 0.85 | 瞳孔最大位移 = (眼径−瞳径)/2 × 0.85，瞳孔永不出眼球 |
| `PUPIL_DRAG_SHRINK` | 0.8 | 拖拽中瞳径 × 0.8（「瞪大」感） |

**瞳孔颜色拍板**（前文档待定项）：深中性色 `Hsla{l:0.06, a:0.88}`，非主题色——瞳孔在白色巩膜上，深色任何主题下都成立；主题色瞳孔在强调色球缘附近有融入风险。**回中缓动拍板**：直接归零（3px 内位移无跳变感，前文档待定项 2）。**眨眼拍板**：v1 做（前文档待定项 3 反转——呼吸+眨眼是「活」的最小组合）。

## 3. 数据与坐标换算

全部来自平台层既有/新增封装，**零新依赖**：

```
指针逻辑坐标 = ( cursor_screen_pos()                    // 设备px，GetCursorPos
                − window_rect(hwnd).左上                // 设备px
                − client_origin_in_window(hwnd) )       // 设备px（无框后≈0）
              ÷ window_scale(hwnd)                      // dpi/96
```

- 球心 = 渲染中已算好的 `(center_x, center_y)`（客户区逻辑坐标，卡片恒居中）
- 半径 = `card_d / 2`（含呼吸当帧值）
- `gaze_offset(指针, 球心, 半径) -> (f32, f32)`：**幅值 ≤ 1 的方向向量**——球心处 (0,0)；球内按距离比例放大（近心小偏、贴边满偏）；出球+容差外 (0,0)。调用方乘以「每眼最大位移」得像素偏移
- 拖拽状态：`ball_being_dragged()`（读现有 `DRAGGING` 静态量的公开封装）

新增平台函数（`platform/windows/mod.rs`，`platform/mod.rs` 导出）：
- `cursor_screen_pos() -> Option<(i32, i32)>`：GetCursorPos 封装，失败返回 None
- `ball_being_dragged() -> bool`：私有 `is_ball_dragging()` 的公开包装

## 4. 动画驱动：自适应 pet tick

**不新增渲染机制**——沿用 800ms 哨兵已验证的 `cx.spawn` + `background_executor().timer()` 循环模式，但**间隔由状态决定**：

```
pet_tick_delay(visible, blinking, hovered):
  !visible          → 800ms（隐藏，不发 notify，与哨兵同拍）
  blink 进行中       → 40ms
  hovered           → 90ms
  其余              → 150ms
```

每拍 `pet_tick`：推进眨眼状态机（到点触发/结束）→ `cx.notify()` → render 以**当帧时刻**重算呼吸相位、眼睑开度、瞳孔偏移。动画值全部是**纯函数**（时间→值），tick 只负责「叫醒」，render 负责「算」——与 morph 动画同一哲学（`advance_morph` 文档注明 render 0.9~2.0ms/帧，timer 唤醒才是抖动源，故采样率而非帧率驱动）。

**功耗预算**：静止 150ms 一拍 ≈ 7 render/s × ≤2ms ≈ 单核 1.4% 上限；hover/眨眼瞬时提频，结束即回落；隐藏时零 notify。与既有 CPU/内存优化方向一致。

**与 800ms 哨兵的关系**：哨兵负责「呈现保活」（整窗失效 + DWM nudge）；pet tick 负责「内容变化」（notify）。两者并行不冲突；nudge 的跳过守卫（拖拽/菜单跟踪）不受影响。

## 5. 渲染规格（`floating.rs` render 内）

1. **呼吸**：`sphere = sphere_diameter_for(total, hovered) × breath_scale(pet_started.elapsed())`——**呼吸只乘进"绘制层"，永不进"布局层"**（2026-10-21 修正：首版把呼吸乘进了 `card_d` 本身，导致字号每拍重新拟合+文字框随 `ball_y` 摆动、docked 窗口沿边缘 ±1px 晃——"文字和球都在动"）。分层口径：
   - **绘制层（呼吸）**：球体圆盘、渐变、光晕模糊、高光——用 `draw_d = card_side_for(sphere × breath, morph)`，同一中心
   - **布局层（静止）**：文字预算与字号拟合、眼睛/刷新按钮位与命中矩形、`card_frac`（dock 数学与贴边补偿）——用 `card_d = card_side_for(sphere, morph)`（未呼吸）
   - 回归测试 `the_breath_only_swallows_the_paint` 钉住：`draw_d` 恒在 `card_d ± BREATH_AMP` 带内、且布局层不随相位变化
2. **眼睛**：替换现有刷新按钮 glyph 区（按钮圆盘、边框、hover 亮度、命中矩形 `set_floating_refresh_hit` **全部不变**，点击仍是刷新）：
   - 圆盘内两只眼睛：`眼径 = (icon_d − 2×PAD − GAP) / 2`，横向排列
   - 巩膜：白 0.92 圆；瞳孔：深色圆，中心 = 眼心 + `gaze × max_shift`（位移随眼睑开度同步衰减）
   - **眨眼**：眼高 × 开度（`rounded_full` 自然变透镜形→闭合线），开度 < 0.35 时瞳孔隐藏
   - **拖拽**：瞳径 × 0.8
   - **刷新中**：整对眼睛退位，原样显示现有 LoaderCircle 旋转（复用已验证路径）
3. **frame_log 取证增强**：帧日志追加 `pet breath=… lid=… gaze=(…,…)`，球径显示呼吸后值——出问题按日志定位（沿用 tm-frame.on 取证开关）

## 6. 几何不变量（呼吸为什么安全）

- **窗口容纳**：呼吸上限 `MAX_SPHERE × HOVER_SCALE × (1+BREATH_AMP)` = 175×1.06×1.015 ≈ 188.3px，其 `clip_diameter_for`（含光晕尾 + 12px CLIP_PAD）仍须 ≤ WINDOW_SIZE(360)——新增不变量测试锁定
- **docked 呼吸**：贴边时球径 ±1.5% → 外缘漂移 ±0.8px，既有「外缘钉在 DOCK_GAP」逻辑按帧补偿（窗口位置取整 1px 级摆动，4s 周期肉眼不可见）；hover pop 6% 早已走同一路径
- **dock 判定**：`card_frac` 随呼吸 ±1.5% 振荡，DOCK_THRESHOLD 32px，无影响
- **刷新命中区**：矩形不变，Win32 子类零改动
- **瞳孔不出界**：位移上限 = (眼径−瞳径)/2×0.85，数学上不可能出巩膜

## 7. 风险与对策

| 风险 | 对策 |
|---|---|
| 拖拽中渲染与 Win32 移窗交错 | 卡片恒居中客户区，render 只读不写窗口几何；哨兵已在拖拽中 notify 的先例 |
| 眨眼采样 40ms 仍粗糙 | 闭眼 140ms ÷ 40ms = 3.5 帧，透镜形渐变可接受；观感不佳再调 TICK_BLINK |
| 隐藏时白白渲染 | `is_floating_visible()` 为 false 跳过 notify |
| 眼睛太小人眼不可辨 | docked 最小球 icon 15px → 眼 4.6px 属下限；参数集中可调（§2 表） |
| 与 z-band nudge / 菜单跟踪互扰 | pet tick 不触碰 z 序；守卫逻辑不新增窗口 |

## 8. 测试计划（纯函数 + 不变量）

- `gaze_at_the_center_is_neutral`：球心 → (0,0)
- `gaze_looks_at_the_pointer_inside_the_ball`：四方向正确、幅值 ≤1、近心小偏贴边满偏
- `gaze_recenters_outside_the_ball`：容差带内仍跟随、带外归零
- `blink_opens_closes_and_reopens`：t=0→1、闭眼中→0、保持段→0、睁开后→1、总时长后→1
- `breathing_stays_within_its_amplitude`：全域 [1−amp, 1+amp]，周期性
- `blink_intervals_vary_within_bounds`：50 次抽样全在 [3.5, 6.5]s 且不全等
- `pet_tick_delays_follow_the_state`：隐藏 800 ≥ 静止 150 ≥ hover 90 ≥ 眨眼 40 映射
- `the_breathing_ball_still_fits_the_window`：呼吸×hover 全域组合的 fit ≤ WINDOW_SIZE（镜像既有 `the_fixed_window_wraps_the_card_and_its_glow`）

## 9. 实施步骤与验收

**步骤**：① 平台层两函数+导出 → ② floating.rs 常量/字段/纯函数 → ③ pet tick 循环 → ④ render 眼睛+呼吸+日志 → ⑤ 测试 → `cargo check --all-targets` / `cargo test --lib` / `cargo fmt --check`。

**验收（人工）**：
- [ ] 鼠标入球：双眼瞳孔跟随；出球 2px 外回中
- [ ] 3.5~6.5s 一次眨眼，320ms 一闭一睁
- [ ] 静止时球 4s 周期 ±1.5% 呼吸，无明显跳帧
- [ ] 拖拽中瞳孔缩小；双击开主窗、贴边 dock、呼吸照常
- [ ] 点眼睛 = 刷新；刷新中 LoaderCircle 旋转（无双眼）
- [ ] docked 方形形态下眼睛/呼吸几何正确
- [ ] 隐藏悬浮窗后 CPU 无 pet 唤醒（任务管理器观察）
- [ ] 既有 203+ 测试全过、无回归

## 10. v2 展望（不做承诺）

- 桌宠开关设置项（球 ↔ 宠物切换）+ 表情包（数据驱动：预算阈值焦虑、深夜揉眼、无活动瞌睡）
- sprite 序列帧角色（需 PNG 渲染确认 + 素材生成）、气泡提醒
- 声音/交互彩蛋、多显示器边缘行走

> AI生成
