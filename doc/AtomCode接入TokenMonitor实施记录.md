# AtomCode 接入 TokenMonitor · 实施记录

> 日期：2026-09-08 · 目标仓库：`E:\project\TokenMonitor`（Rust / GPUI + ratatui 双前端）
> 依据：`doc/增加AtomCode支持_完整指南.md`（§六 数据格式与 6 个坑）+ `doc/atomcode.rs`（字段映射草稿）

## 0. 结论

**TokenMonitor 的架构完全满足 AtomCode 的解析需求，已完成接入。** 第 13 个 provider 落地，`ProviderSource` trait 无需任何改动即可承载 AtomCode 的「双文件 join」形态。

需要澄清一点：`doc/` 下的三份 AtomCode 文档（指南 / 草稿 / 核查）**面向的是 tokscale 与另一个 JS/Electron 项目**，不是本仓库——它们的接线点（`ClientId::AtomCode`、`parse_cached_lane`、`clientsWithIcon`、worker 副本）在 TokenMonitor 里都不存在。本次只复用它们的**数据格式 spec 与坑位处理**，接线按 TokenMonitor 自己的 `ProviderSource` 模式重写。

## 1. 变更清单

| 状态 | 文件 | 改动 |
|---|---|---|
| 新增 | `src/providers/atomcode/mod.rs` | AtomCode 适配器，623 行 / 7 个单测 |
| 新增 | `assets/icons/atomcode.svg` | 多色品牌 logo（源自 `icon-192.svg`，已补 `viewBox="0 0 192 192"` 以支持 16px 缩放；`mono_svg` 仅把白高光重映射为 `currentColor`，蓝填充保留为品牌色） |
| 修改 | `src/core/model/provider.rs` | 枚举加 `AtomCode`；`ALL: [Provider; 12]` → `13`；补 `id()` = `"atomcode"`、`display_name()` = `"AtomCode"` |
| 修改 | `src/providers/mod.rs` | `pub mod atomcode;`；`all_providers() -> [Provider; 13]`；`build_sources()` 加 `Provider::AtomCode` 分支 |
| 修改 | `src/ui/dashboard/card.rs` | `provider_icon_bytes()` 加 `"atomcode"` 分支（否则测试 `every_tracked_provider_has_a_bundled_logo` 失败） |
| 修改 | `Cargo.toml` | `description` 工具清单追加 AtomCode |

其余位置无需改动：`app.rs`、`tui/`、`storage/`、`quota/` 全部通过 `Provider::ALL` 动态迭代，没有硬编码数量；`Pricer::resolve_id` 用 `_ =>` 兜底，非穷举 match。

```text
数据流（与其它 12 个 provider 完全一致）
~/.atomcode/sessions/<hash>/<uuid>.meta  ─┐
~/.atomcode/sessions/<hash>/<uuid>.jsonl ─┴─► AtomCodeSource::scan
                                                 │ emit(UsageRecord)
                                                 ▼
                                      collector → Pricer 打标 cost
                                                 ▼
                                   SQLite (fingerprint UNIQUE 去重)
                                                 ▼
                                        GPUI 卡片 / TUI 报表
```

## 2. 字段映射（统一走 `.meta` 口径）

| TokenMonitor `Usage` | AtomCode 来源 | 说明 |
|---|---|---|
| `model` | `.meta` → `turn_stats[].model_usage[].model_id` | 真实模型名（GLM-5.2 / qwen3.8-27b …） |
| `input_tokens` | `model_usage[].tokens.input` | |
| `output_tokens` | `model_usage[].tokens.output` | |
| `cache_read_tokens` | `model_usage[].tokens.cached_input` | **独立计数，非 input 子集**（坑 4） |
| `cache_write_tokens` | 恒 0 | AtomCode 不上报 |
| `started_at` | `.jsonl` 按 `turn_id` join 取**最早** `ts` | 缺失时回落 `updated_at` → `created_at` → 文件 mtime |
| `cost_micros` | 恒 0，由管线 `Pricer` 打标 | 与其它 adapter 一致 |
| `project` | `.meta` → `working_dir` 的目录名 | 比反推 `<hash>` 目录名更可靠 |
| `session_id` | `.meta` → `id`，空则取文件名 | |
| `fingerprint` | `<rel路径>:<turn_id>:<index>`（legacy 为 `:legacy`） | WSL 根带 label 前缀，跨根不撞车 |

## 3. 六个坑的处理对照

| # | 坑 | 本仓库处理 |
|---|---|---|
| 1 | `.meta` 是整体 JSON，不是 JSONL | 用 `fs::read_to_string` + `serde_json::from_str` 整体解析，**不走** `for_each_line` |
| 2 | `.snapshot` 是陷阱（几十 MB，零 token） | 扩展名白名单只收 `.meta`（`eq_ignore_ascii_case`） |
| 3 | 两文件各持一半信息 | `.meta` 取 token/模型，`.jsonl` 按 `turn_id` join 取时间戳 |
| 4 | `cached_input` 是独立计数 | 映射 `cache_read`，与 `input` 累加 |
| 5 | ~一半轮次是旧格式（无 `model_usage`） | 回落 `used_tokens` 全数计入 `input`，模型标 `unknown`，**不丢数据** |
| 6 | `.meta` / `.jsonl` 数字对不上 | 统一走 meta，jsonl 只取时间戳，不取数字 |

坑 3 是价值最大的一处：`.meta` 只有会话级 `updated_at`，不做 join 会把跨天的几十轮全压到最后一天，日趋势直接失真。

## 4. 与草稿 `doc/atomcode.rs` 的差异

草稿是 tokscale 形态（`UnifiedMessage` / `TokenBreakdown`），本仓库改写为 `UsageRecord` / `Usage`。字段映射逐项对齐，另做 4 处工程化调整：

1. **`raw_bytes` 均摊**：`.meta` 是整体 JSON，没有哪条记录独占字节区间。按 `turn` 数再按 `model_usage` 数均摊，避免每条记录都记整文件长度导致按记录数虚高。
2. **多根发现**：复用 `discover_roots()`，自动覆盖本机 home + Windows 下各 WSL 发行版；根 label 进 fingerprint 命名空间。
3. **指纹短路**：`meta_fingerprint()` 与 `scan_fingerprint()` 共用同一实现，保证「便宜检查」与「全量扫描」口径一致，调度器可跳过无变化的扫描。
4. **错误语义对齐**：畸形 `.meta` 记入 `ScanOutput::errors` 但不中断整轮扫描，与 `scan_roots_inner` 对不可解析文件的既有行为一致。

## 5. 已知偏差（不阻塞功能）

| # | 项 | 影响 | 说明 |
|---|---|---|---|
| 1 | 未解析 `.meta` 的 `pricing` 字段 | `LongCat-2.0` 等冷门模型成本偏高 | 指南提到 `.meta` 自带厂商 pricing。但草稿与指南都没给出该字段的 JSON 结构，为避免臆造未实现。**GLM / Qwen / DeepSeek 不受影响**——`normalize.rs` 的 FAMILY 已覆盖（`glm→z-ai/glm-5`、`qwen→qwen/qwen3.7-max`、`deepseek→deepseek/deepseek-v4-pro`）。未命中的模型按项目既有约定兜底到 `claude-opus-4.8`，与其它 provider 行为一致。若需精确成本，提供一份真实 `.meta` 的 `pricing` 片段即可补上。 |
| 2 | legacy 轮次 `model = "unknown"` | 同上 | 兜底定价偏高，但总 token 量不丢（这是坑 5 的取舍：宁可归到 unknown，也不丢 7 月全部历史） |
| 3 | 跨平台数据路径 | 仅按 `~/.atomcode/sessions` 实现 | macOS/Linux 若路径不同需补 suffix |
| 4 | 未做增量扫描 | 每轮全量解析 `.meta` | 未实现 `scan_incremental`，靠指纹短路避免无变化重扫。`.meta` 单文件不大，暂无必要 |

## 6. 验证

### 本次已完成
- `rustfmt --check` 全部通过（4 个改动文件 + 新文件），语法与格式无问题。

### 待本地补验（本环境无法执行）
沙箱内 `github.com` 走代理返回 502，`gpui-component` 的 git 依赖既无法 fetch、本地 `~/.cargo/git/db` 也不含 `Cargo.lock` 锁定的 `bc174a7e`，`--offline` 同样失败。因此**任何 cargo 命令在依赖解析阶段即失败，与本次代码改动无关**。请在本地网络可达时执行：

```powershell
cargo fmt --check
cargo check --all-targets
cargo test atomcode                 # 本适配器的 7 个单测
cargo test                          # 全量（含 card.rs 的 logo 覆盖断言）
cargo check --bins --no-default-features --features tui
cargo run                           # 确认卡片出现 AtomCode 行且图标正常
```

### 验收基线（取自指南 §五实测）

跑通后可对齐这组数字：

| 模型 | 轮次 | input | output |
|---|---|---|---|
| deepseek-v4-flash | 376 | 12.71M | 2.22M |
| GLM-5.2 | 339 | 11.97M | 800.0K |
| LongCat-2.0 | 178 | 3.53M | 747.4K |
| qwen3.8-27b | 80 | 4.79M | 875.5K |

另外三项定性检查：

- 7 月老会话有数据，model 列显示 `unknown`（坑 5 生效）
- 日趋势没有「某天异常高、前一天为 0」（坑 3 的 join 生效）
- 总量约 1.06B token 量级

> 注意：本仓库 `cache_read` 取 `cached_input`，上表只列 input/output，对齐时以这两列为准。

## 7. 顺带发现

`AGENTS.md` 写「交付物沉淀到 `doc/`」，但仓库同时存在 `docs/`（`code-analysis.md`、`data-sources.md`）。目前 `doc/` 放调研与实施记录、`docs/` 放对外说明，两者并存。建议明确其一或在 `AGENTS.md` 里写清分工，避免后续文档散落。
