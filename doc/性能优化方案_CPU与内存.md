# TokenMonitor 性能优化方案（CPU 与内存）

> 分析对象：当前 `main`（含悬浮球静态光晕版）。
> 目标：降低**扫描期与交互期的 CPU 峰值**、**常驻与峰值内存**，并减少 **SQLite 查询次数 / 线程数**。
> 范围：`collector/`（扫描调度）、`app/`（聚合与状态）、`storage/`（SQL 与 PRAGMA）、`ui/`（渲染）。不含 TUI（结构相同，可套用同一结论）。

---

## 0. 结论速览（按优先级）

| 编号 | 位置 | 问题 | 维度 | 等级 |
|------|------|------|------|------|
| **P0-1** | `app/app.rs:435-461` | `refresh_view` **每次触发都新建线程 + 新开 SQLite 读连接**，无合并/去抖。一次全量扫描（14 个 provider）最多触发 ~14 次「全窗聚合」，每次 5 条聚合 + 365 天报告，图表页再多 28 条 | CPU / 内存 | 🔴 高 |
| **P0-2** | `app/app.rs:92` | `view_tx` / `view_rx` 是 **unbounded** 通道；生产快于消费时快照无背压积压（每个快照含 365 天 + 图表序列） | 内存 | 🔴 高 |
| **P1-1** | `sqlite.rs:26-28`、`usage_repo.rs:270-350` | 按东八「日/小时」聚合用 `date(started_at,'+8 hours')`，**函数套在列上导致索引失效**，窗口内逐行计算 | CPU | 🟠 中高 |
| **P1-2** | `providers/source.rs:65-111, 197-282` | 每 provider 每轮扫描 **walk 文件树两次**（先 `scan_fingerprint` 再 `scan_incremental`） | CPU / IO | 🟠 中高 |
| **P1-3** | `app/app.rs:572-598` | 图表页对 14 个 provider 各查一次（+ 全部时再各查一次模型序列）= **28 条查询/刷新**；`merge_model_series` 为 O(days²) | CPU | 🟠 中 |
| **P1-4** | `ui/report/section.rs:24,195`、`ui/report/heatmap.rs:60-146` | 每帧 **两次 clone 365 天 Vec** + 重建 **365 格热力图**（约 730 个 `Rc<Cell>`）+ 重建 HashMap | CPU / 内存 | 🟠 中 |
| **P2-1** | `collector/mod.rs:113-116, 129-132` | **DB 锁跨越文件 IO**：`run_scan` 在全部 provider 的 walk/parse 期间一直持锁；`scan_async` 每 provider 持锁跨 walk | 并发 / CPU | 🟡 中低 |
| **P2-2** | `scanner.rs:56-82,155-163` | 每轮把整棵 `FileStates`（`HashMap<PathBuf,(mtime,size)>`）**序列化成 JSON** 存进 `settings` 单行，下轮再整棵解析 | 内存 / CPU | 🟡 中低 |
| **P3-1** | `collector/mod.rs:69-78`、`usage_repo.rs:98-141` | 启动时（价格版本变更）`recompute_all_costs` **全表读进 `Vec`** 再回写 | 一次性 | 🟢 低 |
| **P3-2** | `collector/watcher.rs` + `app/app.rs:425-427` | `FileWatcher` 已定义但**未接线**；若接入，`Watch → scan_async` 无去抖会引发扫描风暴 | 未来风险 | 🟢 低 |

---

## 1. 现状数据流与热点位置

```
provider 数据目录 (jsonl / jsonl.zst)
        │  WalkDir + 逐行流式解析
        ▼
  scan_async 线程 ──(持 db 锁)──► SQLite (WAL)  ◄── 独立只读连接 ── aggregate 线程
        │  每 provider: ScanStarted/ScanCompleted                    ▲
        ▼                                                           │ refresh_view 每次新建
  CollectorEvent  ──► handle_collector_event ──► refresh_view ──────┘ 线程 + 连接
        │                                    ▲
        │  (ScanCompleted 若 !unchanged)      └── view_tx: UNBOUNDED 通道
        ▼                                                 │
   UI 事件循环 (apply_snapshot) ◄── view_rx ◄─────────────┘
        │
        ▼
   Render → dashboard → report_section → heatmap (每帧重建 365 格)
                     └► floating ball (set_totals + notify)
```

**两个最贵的点：**
1. **聚合放大器**：`refresh_view` 无合并 → 扫描期「provider 数 × 单次全窗聚合」次计算，且每次开新连接、起新线程。
2. **无背压**：`view_tx` 无界，聚合比 UI 应用快时，大快照在通道里堆积。

---

## 2. CPU 侧问题与优化

### P0-1 `refresh_view`：合并请求 + 常驻 worker + 复用连接

**现状**（`app/app.rs:435-461`）：

```rust
fn refresh_view(&mut self, _cx: &mut Context<Self>) {
    let seq = self.view_seq.wrapping_add(1);
    self.view_seq = seq;
    // ... 拼 window / charts / report ...
    std::thread::Builder::new()
        .name("tokenmonitor-aggregate".into())
        .spawn(move || {                       // ← 每次请求起一条线程
            let snapshot = compute_view_snapshot(seq, time_tab, &db_path, window, charts, report);
            let _ = tx.send_blocking(snapshot);
        })
        .expect("spawn aggregate thread");
}
```

问题：
- **每次调用 = 1 线程 + 1 新 `Connection`**（`sqlite::open_read` 里的 open + pragma + WAL 协商）。
- 触发点密集：`ScanCompleted`（每个 provider 一次）、`Watch → scan_async`、切页/切标签/切范围。全量扫描一轮最多 ~14 次。
- **没有合并**：14 次触发 = 14 次「全窗 5 条聚合 + 365 天报告 +（图表页）28 条查询」。

**优化**：单条常驻聚合 worker + **bounded(1) 的「最新请求」通道** + 复用一个只读连接；队列满就只保留最新请求（dirty-latest-wins）。

```rust
// 请求只保留“最新”：bounded(1)，try_send 失败即视为已有一个待处理请求
fn refresh_view(&mut self, _cx: &mut Context<Self>) {
    let req = AggRequest { seq: next_seq(), time_tab, window, charts, report };
    let _ = self.agg_tx.try_send(req);   // 满 → 丢弃本次（旧的仍在排队）
    // 若希望“最后一个请求一定要算”，再加一个 AtomicBool dirty 标志，worker 结束时回查
}

// 常驻 worker（在 App::new 起一次）
fn agg_worker(req_rx: Receiver<AggRequest>, out_tx: Sender<ViewSnapshot>, db_path: PathBuf) {
    let conn = sqlite::open_read(&db_path).expect("open read db"); // ← 复用，不再每次 open
    loop {
        let req = match req_rx.recv_blocking() { Ok(r) => r, Err(_) => break };
        let snapshot = compute_view_snapshot(&conn, req);           // ← 传 &Connection
        let _ = out_tx.try_send(snapshot);                          // bounded(1)：只保留最新
    }
}
```

配套：`compute_view_snapshot` 改为接收 `&Connection`（去掉内部 `open_read`），并在 `apply_snapshot` 前加一层「同 seq 去重」。

> 备选（更小改动）：保留 `compute_view_snapshot` 现状，但在 `handle_collector_event` 里**去抖**——把 300ms 内的多个 `ScanCompleted` 合并成一次 `refresh_view`。收益略小但改动最小。

**预期**：扫描期聚合次数 **14 → 1~2**；聚合线程数 **N → 1**；连接 open 次数 **N → 1**。

---

### P1-1 东八日/小时聚合：用持久列 + 索引替换 `date()` 函数

**现状**（`usage_repo.rs`）：`aggregate_by_day` / `daily_series*` / `stats_by_hour` 全部

```sql
GROUP BY date(started_at, '+8 hours')      -- 列上套函数 → 该表达式无法用索引
```

`WHERE started_at >= ?1 AND started_at < ?2` 能走 `idx_usage_*_started`，但分组要对窗口内**每一行**计算一次 `date()`。365 天报告窗口在大库上是重扫，且被 P0-1 放大 14 倍。

**优化**：加**生成列 + 索引**（SQLite 3.31+ 支持对 VIRTUAL 生成列建索引；`rusqlite` bundled 版本满足）。

```sql
ALTER TABLE usage_records
  ADD COLUMN day_key  TEXT    GENERATED ALWAYS AS (date(started_at,'+8 hours')) VIRTUAL;
ALTER TABLE usage_records
  ADD COLUMN hour_key INTEGER GENERATED ALWAYS AS
         (CAST(strftime('%H', started_at,'+8 hours') AS INTEGER)) VIRTUAL;

CREATE INDEX IF NOT EXISTS idx_usage_day           ON usage_records(day_key);
CREATE INDEX IF NOT EXISTS idx_usage_provider_day  ON usage_records(provider, day_key);
CREATE INDEX IF NOT EXISTS idx_usage_project_day   ON usage_records(project,  day_key);
CREATE INDEX IF NOT EXISTS idx_usage_model_day     ON usage_records(model,    day_key);
```

然后所有查询改为 `GROUP BY day_key`，按 provider 过滤的改用 `(provider, day_key)` 索引；小时用 `GROUP BY hour_key` 且 `WHERE day_key = ?1`。

> 若不便用生成列（老 SQLite/迁移顾虑），退一步：在插入时由 Rust 侧显式写入 `day_key` / `hour_key` 普通列（Rust 已有 `east8_local`，无需 DB 计算），索引与查询同上。

---

### P1-2 扫描：消除「双 walk」

**现状**（`providers/source.rs`）：`scan_one`（`scanner.rs:56-73`）先 `source.scan_fingerprint()` 走一遍 `WalkDir` 统计 `found/max_mtime/total_bytes`；随后 `scan_incremental()` 再走一遍。

- 指纹那次 walk 本身**几乎和扫描 walk 一样贵**（同样 `WalkDir` + 每文件 `metadata`），「廉价预检」名不副实。
- WSL 的 `\\wsl.localhost\...`（9P）根目录上代价更高。

**优化**：去掉前置指纹 walk，只走一次——直接 `scan_incremental`（其内部对未变文件只 `stat`、对变更文件才解析），用其返回的 `ScanOutput.fingerprint` 与持久化值比对来判定 `unchanged`：

```rust
// scanner.rs：去掉 scan_fingerprint 预检
let output = source.scan_incremental(&mut |r| { /* 解析+计费+批量入库 */ }, &known)?;
let unchanged = settings.get(&fp_key)? == Some(output.fingerprint.clone())
                && summary.records == 0;      // 没解析出任何记录 ⇒ 等价于 unchanged
```

（注意：per-file states 命中时本来就不解析，walk 只做 `stat`，因此合并为一次 walk 是纯收益。）

---

### P1-3 图表页：28 条查询 → 2 条；去掉 O(n²) 合并

**现状**（`app/app.rs:572-598`）：`compute_chart_snapshot` 对 `Provider::ALL`（14 个）逐个 `daily_series_by_provider`；`ChartApp::All` 再逐个 `daily_series_by_provider_model`。

**优化**：
- 一次查询取回全部 provider 的日序列：
  ```sql
  SELECT provider, day_key, COUNT(*), SUM(input_tokens), ... , SUM(cost_micros)
  FROM usage_records
  WHERE started_at >= ?1 AND started_at < ?2
  GROUP BY provider, day_key ORDER BY provider, day_key;
  ```
  在 Rust 侧按 provider 拆分。模型序列同理改为 `GROUP BY model, day_key`（或 `provider, model, day_key`）。
- `merge_model_series`（`app.rs:602-615`）用 `entry.iter_mut().find(...)` 是 **O(days²)**；改为 `BTreeMap<day, SumStats>` 再收集，O(days·log days)。

**预期**：图表页刷新 SQL 次数 **28 → 2**；合并从 O(days²) → O(days log days)。

---

### P1-4 渲染侧：去掉每帧 clone，缓存热力图派生数据

**现状**：
- `ui/report/section.rs:24`：`let data = app.state.report.data.clone();` — 每次渲染整棵 365 天 `Vec` clone。
- `ui/report/section.rs:195`：`ContributionHeatmap::new(days.to_vec())` — **再 clone 一次**。
- `ui/report/heatmap.rs:72`：每次渲染重建 `HashMap<NaiveDate, SumStats>`。
- `heatmap.rs:86-115`：每次重建 53 列 × 7 行 = **365 个 cell**；`heatmap.rs:193-194` 每 cell 新建 2 个 `Rc<Cell>` → 每帧 ~730 次 Rc 分配。
- hover 回调（`section.rs:54-70`）在 `report_hover` 变化时 `cx.notify()` → **整页重绘**（含上述全部重建）。鼠标划过热力图会逐格触发。

**优化**：
1. `report_section` 改为**借用** `&app.state.report.data`（或只取出需要的字段），避免整体 clone。
2. `ContributionHeatmap::new(days)` 接收 `&[(NaiveDate, SumStats)]`（或 `Rc<Vec<..>>`），不再 `to_vec()`。
3. 把 `day → SumStats` 的 `HashMap`、以及每格的 `level` 在**快照生成时预计算一次**，存进 `ReportSnapshot`；render 只查表，不重建。
4. （可选，收益最大）把 tooltip 的 hover 状态**下沉到独立子实体**，hover 只重绘热力图卡片，而不是整个 dashboard 实体。

---

### P2-1 收窄 DB 锁范围（不要在文件 IO 期间持锁）

**现状**：
- `collector/mod.rs:113-115` `run_scan` 在 `scan_all`（**所有** provider 的 walk/parse）全程持 `db` 锁。
- `collector/mod.rs:129-132` `scan_async` 每 provider 在 `scan_one`（含 walk）期间持锁。

后果：扫描时 `collector.get_*/set_*`（悬浮窗可见性、主题色、扫描间隔等）会被长时间阻塞；两次并发扫描互相串行等待。

**优化**：把锁粒度收到「写批次」那一刻。让 `scan_one` 接受 flush 回调或 `&Mutex<Connection>`，只在 `batch_insert_dedup` 时短暂加锁：

```rust
// 概念：walk/parse 不持锁，flush 时才锁
let mut flush = |batch: &[UsageRecord]| -> Result<BatchInsertStats> {
    let conn = db.lock().unwrap();
    UsageRepo::new(&conn).batch_insert_dedup(batch)
};
scan_one_streaming(source, &mut flush)?;
```

---

## 3. 内存侧问题与优化

### P0-2 有界通道 + 最新快照槽位

**现状**（`app/app.rs:92`）：`let (view_tx, view_rx) = unbounded();`。`ViewSnapshot` 含 `by_provider / by_provider_model / by_project / by_day / charts / report(365天)`，单个可能几十~上百 KB；扫描期集中产出极易积压。

**优化**：
- 通道改 **`bounded(1)`**；配合 P0-1 的「最新请求合并」，通道里最多 1 个待应用快照。
- 或改用 **`Arc<Mutex<Option<ViewSnapshot>>>` + 事件通知**的「最新值槽位」：生产端覆盖写，消费端取走，天然无积压。

```rust
// 最新值槽位
let slot: Arc<Mutex<Option<ViewSnapshot>>> = Arc::new(Mutex::new(None));
// 生产：*slot.lock().unwrap() = Some(snapshot); wake.send(());
// 消费：if let Some(s) = slot.lock().unwrap().take() { apply(s) }
```

---

### P2-2 `FileStates` 从 settings JSON 迁到独立表

**现状**（`scanner.rs:56-82, 155-163`）：每轮 `settings.get("scan.files.<provider>")` → `serde_json::from_str` 反序列化**整棵** `HashMap<PathBuf,(i64,u64)>`；扫描后 `serde_json::to_string` 再写回。文件多时（数万 session 文件）该 JSON 达数百 KB~MB 级，**每 provider 每轮**全量解析/序列化，且塞在 `settings` 单行 TEXT 里。

**优化**：独立表 + 增量 UPSERT：

```sql
CREATE TABLE IF NOT EXISTS scan_file_state(
  provider TEXT NOT NULL,
  path     TEXT NOT NULL,
  mtime    INTEGER NOT NULL,
  size     INTEGER NOT NULL,
  PRIMARY KEY (provider, path)
);
```

- 已知状态：`SELECT path, mtime, size FROM scan_file_state WHERE provider = ?`（或按需分页/游标）。
- 变更文件：`INSERT ... ON CONFLICT(provider,path) DO UPDATE SET mtime=excluded.mtime, size=excluded.size`。
- 删除已消失文件：本轮收集到的 path 集合做一次差集清理（或延时清理）。

**预期**：去掉每轮对整棵文件树状态的 JSON 解析/序列化；`settings` 表不再放大。

---

### P3-1 `recompute_all_costs` 流式化（一次性）

`usage_repo.rs:98-141` 把全表 `(id,cost)` 先读进 `Vec`（~16B/行）再分批回写。大表时是启动内存峰值。改为**游标边读边写**（或 `LIMIT/OFFSET` 分页）即可，收益一次性。

---

## 4. 建议实施顺序

| 阶段 | 改动 | 风险 | 预期收益 |
|------|------|------|----------|
| **阶段 1（低风险、高收益）** | P0-1 合并刷新 + 常驻 worker + 复用连接；P0-2 `bounded(1)`/最新值槽位；P1-4 去掉每帧 clone | 低（局部重构） | 扫描期 CPU 峰值与线程数大幅下降；内存峰值回落 |
| **阶段 2（SQL）** | P1-1 `day_key/hour_key` 列 + 索引；P1-3 合并图表查询 + 去 O(n²) | 中（需迁移） | 365 天报告与图表查询由「逐行算函数」变索引扫描 |
| **阶段 3（扫描/存储）** | P1-2 单次 walk；P2-1 收窄锁；P2-2 `scan_file_state` 表化 | 中（触及扫描主链路） | IO 减半、扫描期不再长占锁、去掉大 JSON 抖动 |
| **阶段 4（可选）** | P3-1 流式回填；P3-2 watcher 去抖后再接线；热力图 hover 子实体隔离 | 低~中 | 进一步压榨峰值 |

---

## 5. 验收与度量

- **构建**：`cargo build --release`（`lto=true`、`codegen-units=1` 已开）。
- **观测**：任务管理器看 CPU/RSS 峰值；对「空闲 / 手动扫描 / 切到图表页」三种场景各测一次。
- **临时计数（建议临时加，验收后移除）**：`refresh_view` 触发次数、聚合线程并发数、单次 `compute_view_snapshot` 的 SQL 条数与耗时、`view_rx` 通道积压深度。
- **基准对比**：单次全量扫描下（a）聚合次数、（b）SQL 总时间、（c）RSS 峰值、（d）聚合线程峰值。
- 回归：`cargo test`（当前 148 passed）+ `cargo check --features ui` / `--no-default-features --features tui` 均需保持零告警。

---

## 附：本次未改动代码，仅分析

上文所有行号引用基于当前 `main`。实施阶段 1 与阶段 2 可独立提交、互不阻塞；阶段 3 会触及扫描主链路，建议单独分支验证。
