# 图标变纯白方块：根因与 SVG 规范

## 现象

2026-11-23：换机装 0.3.15 后，界面卡片前的 **CodeArts / AtomCode** 小图标变成纯白实心块（形状不可辨）。本机开发版看起来正常。

## 根因（不是颜色，是 SVG 结构）

GPUI 的 `svg()` 元素**天生只能渲染单色 alpha mask**：

- `gpui/src/elements/svg.rs:150`：只有设置了 `text_color` 才调用 `paint_svg`，否则不绘制。
- `gpui/src/window.rs::paint_svg`：先把 SVG 转成**灰度 alpha mask（丢弃全部原始 fill 颜色）**，再用 `text_color` 单色上色。

所以 SVG 的 `fill="#hex"` 颜色一律被当作"这个像素是否不透明"来用。**只要图形在 alpha 通道上是实心铺满的，渲染结果就是纯白（前景色）实心块**，`mono_svg()` 替换颜色也救不了形状问题。

### 本次的直接原因

`codearts.svg`（11-18 提交 `565cd12` 引入，21026 字节官方导出图）含：

```xml
<filter id="pixso_custom_mask_type_alpha">
  <feColorMatrix type="matrix"
    values="0 0 0 0 1  0 0 0 0 1  0 0 0 0 1  0 0 0 1 0"/>
</filter>
<g filter="url(#pixso_custom_mask_type_alpha)"> ... </g>
```

那个 `feColorMatrix` 的 RGB 行末位全是 `1` = **把整个图形强制染白**，加上 6 个 `linearGradient` 和铺满的 path，alpha mask 是不透明块 → 白方块。

**关键时间线**：11-16 已把 codearts 换成 172 字节镂空 `< / >` 符号修好过，但 11-18 的 `565cd12`（embed screen push + Trae ingest）把它覆盖回官方彩色图 → 缺陷复活。本机当时跑的是 11-16 编译的旧 exe，所以没暴露；换机装新 MSI 就显形。

## 修复（0.3.16）

两个图标都改成**镂空单色 path**，与其他 50 个正常图标同款格式：

| 图标 | 修复后 | 结构 |
|---|---|---|
| `codearts.svg` | `< / >` 代码符号 | 3 个 `<path>`，`fill="currentColor"`，镂空 |
| `atomcode.svg` | 原子轨道符号（11-16 版本，未动） | 3 个 `<ellipse>` + `<circle>`，`stroke="currentColor" fill="none"` |

用无头 Chrome 把两个 SVG 按 `mask-image` 方式染色验证（等价 GPUI 的 alpha mask 流程）：都呈现清晰镂空符号，**不是实心块**。

## SVG 规范（防止再犯）

放进 `assets/icons/` 的 SVG 必须满足：

1. **alpha 通道镂空**：图形要有明确的空心/描边，`fill="none"` 或 currentColor path 之间留白；禁止铺满 viewBox 的实心 rect/path（位图描摹图 `C1.04978...` 这种浮点残差特征的一律不要）。
2. **禁止 `<filter>` / `feColorMatrix`**：SVG 渲染后端对不同 filter 支持不一致，`feColorMatrix` 染白是本次直接祸首。
3. **禁止 `<linearGradient>` / `<radialGradient>`**：GPUI 会丢弃颜色，渐变无意义且引入后端差异。
4. **用 `currentColor` 或 `stroke="currentColor"`**：`mono_svg()` 会把 `#fff/#ffffff/white` 换成 currentColor，但其它硬编码色不换；统一写 currentColor 最稳。
5. **优先单 `<path>` 实心符号**（如 trae/claude/codebuddy）：项目里 50 个这类图标从没出过白块，是最稳格式。

### 已知同类隐患（未暴露，暂未改）

工作日志 11-16 记录过、`0 currentColor` + 彩色实心 fill 的图标：`cherrystudio` / `doubao` / `kimi` / `icon-192` / `newapi` / `os-apple` / `os-linux` / `os-windows` / `pi` / `reasonix` / `volcengine` / `xiaomi` 等。它们的 path 是"字母/形状"而非铺满矩形，目前显示为形状轮廓（可接受），但若将来某次渲染后端变化可能暴露。真要根治，按上述规范逐个换成镂空/单 path currentColor 版。

## 排查方法

1. 看 icon 的 SVG：`filter=` / `feColor` / `linearGradient` / 铺满 path = 白块高危。
2. 无头 Chrome + `mask-image: url(data:image/svg+xml;base64,...)` 渲染，等价 GPUI alpha mask 流程，肉眼确认是否镂空。
3. 换机差异通常不是 SVG 内容（编译进 exe，两台一样），而是**版本不同**（旧 MSI 含旧图标）或**主题 foreground**。先确认装的 MSI 版本。
