# TokenMonitor 代码分析报告

> 分析对象：`E:\project\TokenMonitor`（v0.3.4，Rust 1.95.0，单 crate + feature 门控双前端）
> 分析方式：静态阅读核心/采集/存储/provider 层源码 + 依赖方向校验 + 测试分布统计 + 后台 `cargo check` 构建校验
> 结论先行：**架构分层清晰、领域逻辑（去重/聚合/定价/时窗）严谨且有单测覆盖、采集管线设计成熟；主要偏差集中在"生产模块 ≤ 500 行"约定被大面积突破，以及少数生产路径的 `unwrap`/双实现一致性隐患。**

---

## 1. 总体健康度

| 维度 | 结论 | 证据 |
|------|------|------|
| 分层架构 | ✅ 合规 | `core` 无反向依赖 `ui/app/collector/storage`；`providers` 不引用 `ui/app`（grep 校验为空） |
| 领域正确性 | ✅ 扎实 | 去重键 UNIQUE + `INSERT OR IGNORE`；定价回退链有单测；东八区时窗与 tokei 一致 |
| 采集管线 | ✅ 成熟 | 指纹短路 + 增量文件状态 + 流式批插 + crash-safe 持久化顺序 |
| 测试覆盖 | ✅ 良好 | 169 个 `#[test]`/`#[cfg(test)]` 标记；core 与 providers 均有单测 |
| 模块体量 | ⚠️ 偏差 | 12 个生产文件 > 500 行（违反 AGENTS.md 约定） |
| 生产健壮性 | ⚠️ 局部 | 少量生产路径 `unwrap`/`expect`、单条 `insert` 缺 OR IGNORE |
| 构建状态 | 🔄 校验中 | 后台 `cargo check --all-targets` 仍在拉取 GPUI(zed) git 依赖 |

---

## 2. 架构与分层（对照 AGENTS.md）

依赖方向严格单向：`core`(领域) ← `providers`/`storage`(适配) ← `collector`(编排) ← 前端(`app`+`ui` GPUI / `tui` ratatui)。

```
core (无渲染依赖, 可单测)
  ▲
  │ providers / storage
  ▲
  │ collector (scan / watch / scheduler)
  ▲
  │ app+ui (GPUI)  ──feature "ui"──    tui (ratatui) ──feature "tui"──
```

- `lib.rs:15-28`：仅 `app`/`ui`/`tui` 受 feature 门控声明，符合"双前端互斥"约定。
- `Cargo.toml:78-87`：GPUI 栈全部 `optional`、同源 git、**未固定 `rev=`**，符合"TUI 构建不拖入 gpui"约束。
- bin 命名 `tokenmonitor-app`（`Cargo.toml:21` 注释）规避 Intel GPU 驱动按 exe 名分配的 ~150MB 内存开销——工程细节到位。

**偏差**：AGENTS.md 明确要求"生产模块 ≤ 500 行"，实际有 12 个文件突破（见第 5 节）。这是与既定约定最大的背离。

---

## 3. 领域核心正确性

### 3.1 去重（dedup）✅
- `storage/sqlite.rs:23`：`fingerprint TEXT NOT NULL UNIQUE`。
- `usage_repo.rs:62`：`INSERT OR IGNORE`，配合 `affected>0` 计插入/跳过（`usage_repo.rs:83-87`），统计项清晰。
- `UsageRecord.fingerprint` 由 provider 适配器设置（`usage_record.rs:15`），多根扫描用 `ScanRoot.label` 命名空间前缀避免跨根碰撞（`source.rs:54-61`）。

### 3.2 定价（pricer）✅
- `pricer.rs:65-92`：`cost_micros` 与 tokei 对齐；Codex 高上下文（`>272_000` 输入+缓存读）双倍输入/缓存读、1.5× 输出、且不计缓存写（`pricer.rs:77-84`，注释引用 tokei）。
- 回退链：alias → normalize → gemini pro/flash → family keyword → 保守兜底（Codex→gpt-5.5，其他→opus）（`pricer.rs:95-125`）。空/`"<synthetic>"` 返回 `None` → 成本 0，不静默记错。
- 单测齐全：`claude_linear_cost` / `codex_high_context_surcharge` / `unknown_glm_falls_back_to_a_priced_model` 等（`pricer.rs:128-238`）。

### 3.3 时窗与东八区 ✅（但双实现有维护风险）
- 全部聚合以**东八区墙钟日**为界：`window.rs:13` `day_key` 用 `east8()`；SQL 侧 `date(started_at, '+8 hours')`（`usage_repo.rs:271,278,323,370`）。
- `tests/domain.rs:111-131` 显式断言 `2026-08-14 12:00 UTC → day.start = 08-13 16:00 UTC`（即东八区 08-14 00:00），逻辑自洽。
- ⚠️ **风险**：日界有两套独立实现（Rust `day_key` vs SQL `+8h`），靠注释"必须保持 lockstep"约束（`usage_repo.rs:267-269`）。一旦漂移，内存聚合（TUI/核心 `by_day`）与 SQL 聚合（UI `aggregate_by_day`）结果会不一致。建议抽单一真值源 + 跨实现同值断言测试。

### 3.4 聚合 ✅
- `aggregation/group.rs`：所有 `by_*` 均经 `sorted_by_cost_desc` 按成本降序（`group.rs:99-103`），与 tokei 一致；`total`/`by_provider_model` 单元测试覆盖（`group.rs:105-158`）。
- `SumStats` 全程 `saturating_add`，无溢出风险（`sum_stats.rs:36-47`）。

---

## 4. 采集 / 持久化管线（scanner）✅ 设计成熟

`scanner.rs:45-167` 的 `scan_one` 体现多处工程成熟度：

1. **指纹短路**（`scanner.rs:68-73`）：源树未变则完全跳过读/解析/写。
2. **增量文件状态**（`scanner.rs:75-82`）：按 `(mtime,size)` 跳过未变文件；状态损坏降级为全量扫描，绝不丢数据。
3. **流式批插**（`scanner.rs:84-122`）：`INSERT_BATCH=2000` 分批，扫描全程不持有全量记录；定价在插入前由管线打 `cost_micros`（`scanner.rs:98-108`）。
4. **crash-safe 持久化顺序**（`scanner.rs:152-163`）：先持久化文件状态、再持久化指纹；中途崩溃只重做工作、绝不漏数据（注释明确）。
5. **错误隔离**：工具未安装 = `DataDirNotFound` → 非错误返回空摘要（`scanner.rs:61,127`）。

`Collector` 打开时做幂等回填：`recompute_all_costs`（定价版本号驱动）+ CodeBuddy 指纹迁移（`collector/mod.rs:60-106`），均为一次性、可中断重跑的设计。

---

## 5. 模块体量偏差（约定冲突）

AGENTS.md："生产模块 ≤ 500 行"。实际超标的 13 个文件：

| 文件 | 行数 |
|------|------|
| `providers/workbuddy/mod.rs` | 856 |
| `tui/ui.rs` | 822 |
| `providers/openclaw/mod.rs` | 762 |
| `providers/antigravity/mod.rs` | 706 |
| `providers/opencode/mod.rs` | 683 |
| `providers/deepseek/mod.rs` | 647 |
| `app/app.rs` | 635 |
| `providers/atomcode/mod.rs` | 623 |
| `providers/codebuddy/mod.rs` | 605 |
| `core/update.rs` | 558 |
| `tui/app.rs` | 529 |
| `providers/pi/mod.rs` | 514 |
| `ui/report/heatmap.rs` | 506 |

> 多数超大文件集中在 `providers/*`（各 AI 工具本地数据格式各异，解析+归一化天然臃肿）。属"合理但未守约"，建议二选一：① 把约定放宽为"provider 适配器 ≤ 800 行"；② 将每个 provider 拆为 `parse`（格式解析）/ `normalize`（归一化）/ `convert`（格式转换）子模块，降到 500 行内。

---

## 6. 风险与改进清单

| # | 类别 | 位置 | 问题 | 严重度 | 建议 |
|---|------|------|------|--------|------|
| 1 | 约定 | §5 所列 12 文件 | 违反"生产模块 ≤ 500 行" | 中 | 拆分 provider / 放宽约定，二选一 |
| 2 | 健壮性 | `providers/antigravity/mod.rs:482-483` | 生产路径 `create_dir_all().unwrap()` / `Connection::open().unwrap()`，权限错误会 panic 扫描线程 | 中 | 改 `anyhow::Result` + `?` 传播，错误进入 `ScanOutput.errors` |
| 3 | 隐患 | `storage/repository/usage_repo.rs:28` | 单条 `insert` 用普通 `INSERT`，遇重复 `fingerprint` 触发 UNIQUE 约束错误；scanner 已走 `batch_insert_dedup`，该单条方法疑似未被生产调用 | 低 | 确认是否已死代码；若保留，加 `OR IGNORE` 或文档约束"调用方须保证唯一" |
| 4 | 可维护性 | 东八区日界：`window.rs:13` vs `usage_repo.rs:271/278/323/370` | 双实现必须 lockstep，漂移导致内存聚合与 SQL 聚合结果不一致 | 低-中 | 抽单一真值源函数 + 跨实现同值断言 |
| 5 | 健壮性 | `core/update.rs:464-500` | `select_asset_for_os(...).expect("...")` 在更新下载路径，某 OS 缺资产会 panic | 低 | 改 `Result` 返回，UI 层降级提示 |
| 6 | 工程 | 未见 `.github/` CI | AGENTS.md 校验命令依赖人工执行 | 低 | 加 CI 跑 `cargo fmt --check`/`check`/`test`/`tree -d` |
| 7 | 文档约定 | 交付目录 | AGENTS.md 写"沉淀到 doc/"，但仓库实际目录为 `docs/` | 低 | 统一口径（本文已按实际 `docs/` 存放） |

**正面项（值得保留的模式）**：
- `unsafe` 仅出现在 `platform/windows/*`（Win32 暗色模式 + 托盘 FFI），范围收敛、可控。
- `recompute_all_costs` 只持 `(id, cost)` 二元组、分批写，崩溃可安全重跑（`usage_repo.rs:98-141`）。
- 全链路 `saturating_add` / `COALESCE(SUM(...),0)`，无溢出与 NULL 风险。
- 测试中对时区边界、定价回退、去重幂等有明确断言。

---

## 7. 构建 / 质量门禁状态 🔴 环境阻断（非代码问题）

工具链自检通过，但**编译验证被沙箱网络阻断**：

| 命令 | 结果 |
|------|------|
| `rustup show` | ✅ 1.95.0-x86_64-pc-windows-msvc active，与 `rust-toolchain.toml` 一致 |
| `cargo check --all-targets` | ❌ 依赖解析阶段失败 |
| `cargo check --no-default-features --features tui` | ❌ 同样在解析阶段失败（cargo 解析全图含可选 git 源） |

失败根因（来自 `/tmp/cargo_check.log`）：

```text
Updating git repository `https://github.com/longbridge/gpui-component`
warning: spurious network error ... SSL connect error / CONNECT tunnel failed, response 502
error: failed to get `gpui-component` as a dependency
Caused by: revision bc174a7ec4534b2a4174fddde314b38d30d69093 not found
Caused by: network failure
```

- 本沙箱对 `github.com` 走代理且返回 **502**，无法克隆 `zed` / `gpui-component` 两个 git 依赖。
- `crates.io` 镜像（tuna）可达（索引更新成功），但两个 git 源不可达，而 cargo 解析依赖图时必须更新它们，**导致任何 `cargo` 子命令都无法越过解析阶段**——与代码正确性无关。
- `cargo fmt --check` / `cargo test` / `cargo tree -d` 同样会卡在同一步，故本次无法执行。

**如何在你的环境补验**（需可访问 github.com 的网络，或预热的 `~/.cargo/git` 缓存）：

```powershell
cargo fmt --check
cargo check --all-targets
cargo test
cargo tree -d                              # 确认单一 gpui / gpui-component 来源、无 rev 分叉
cargo check --bins --no-default-features --features tui
cargo tree --no-default-features --features tui --edges normal,build   # 确认 TUI 图无 gpui
```

> 静态分析（第 2–6 节）不依赖编译，结论独立成立。一旦网络可达，建议按上表跑一遍门禁以闭环。

---

## 8. 建议优先级

1. **P1（正确性维护）**：消除第 6 节 #4 东八区双实现漂移风险（最隐蔽、最难排查）。
2. **P2（健壮性）**：#2 / #5 的生产路径 `unwrap`/`expect` 改为 `Result`。
3. **P2（约定）**：#1 模块体量——明确"守约 or 放宽"，并统一 `doc/` vs `docs/`。
4. **P3（工程）**：#3 确认单条 `insert` 是否死代码；#6 补 CI。

---

_分析基于源码静态阅读与依赖/测试统计，构建结果待后台 `cargo check` 完成后回填。_
