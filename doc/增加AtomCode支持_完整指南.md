# 给 Token Monitor 增加 AtomCode 支持

> 基于 2026-09-01 的 upstream 代码核对：`junhoyeo/tokscale` 的 `tokscale-core`、
> `Javis603/token-monitor` 的 `main` 分支，以及你本机 `~/.atomcode/` 的真实数据。

---

## 一、先说一个反直觉的事实：只 fork token-monitor 改 JS 是没用的

这是最容易走错的岔路，先讲清楚。

Token Monitor 是 Electron 应用（TS/JS），但它**自己不解析任何日志**。全部解析工作
外包给了一个 Rust 写的 CLI —— **tokscale**，而且是以**预编译二进制**形式通过 npm 安装的：

```
@tokscale/cli-win32-x64-msvc     ← 你在 Windows 上实际用的那个
@tokscale/cli-darwin-arm64
@tokscale/cli-linux-x64-gnu
... 共 9 个平台包
```

证据在 `scripts/vendor/tokscale.json`：

```json
{
  "mode": "upstream",
  "upstream": "junhoyeo/tokscale",
  "fork": "Javis603/tokscale",
  "baseVersion": "4.15.0",
  "platforms": {
    "win32-x64": {
      "package": "@tokscale/cli-win32-x64-msvc",
      "asset": "tokscale-win32-x64.exe",
      "sha256": "75e82e00c029b67115de3fac5332b17fec81f73217d5fc481c41011449e87cb0"
    }
  }
}
```

所以"加一个厂商"这件事，**99% 的工作量在 Rust 侧**：改 tokscale 源码 → 重新编译 → 出新二进制。
token-monitor 这边只需要改一两行字符串。

**好消息**：token-monitor 的能力探测是动态的（`src/shared/tokscaleCapabilities.js` 会去解析
`tokscale --help` 里 `--client` 的 possible values），所以二进制一旦认识 `atomcode`，
Electron 侧会自动发现它，**不需要维护白名单**。

---

## 二、两条路线，我推荐第一条

### 路线 A：给 tokscale 上游提 PR（推荐）

```
你改 Rust 代码 → 提 PR 给 junhoyeo/tokscale → 合并后官方发版
→ token-monitor 升级 baseVersion → 所有用户自动获得
```

- 成本：**只需写代码**，编译发版由上游 CI 负责
- 门槛：通过 code review（他们要求带单元测试）
- 周期：通常 1–3 周
- 附加收益：你的解析器由上游维护，AtomCode 改格式时不用自己跟

社区对国产工具的适配是欢迎的 —— `Kimi = 9`、`Qwen = 10`、`CodeBuddy = 35`、
`WorkBuddy = 36` 都是这么进来的。

### 路线 B：自己 fork + 编译 + 发版

```
fork tokscale → 改代码 → 本地编译 9 个平台 → 发 GitHub Release
→ 改 token-monitor 的 vendor/tokscale.json 指向你的 release
```

- 成本：**重**。要配 9 平台的交叉编译工具链（macOS 二进制必须在 macOS 上编，
  Windows 的 MSVC target 也需要对应环境），通常得靠 GitHub Actions 矩阵构建
- 门槛：持续维护 —— token-monitor 每次升级 `baseVersion`，你的 pin 就失效，要重新对齐
- 适用：上游迟迟不合并，你又急着用

下面的改动说明对两条路线都适用。

---

## 三、tokscale 侧：4 处改动

### 改动 1：新建解析器 `crates/tokscale-core/src/sessions/atomcode.rs`

直接用仓库里的 `tokscale_atomcode/atomcode.rs`（已写好，含 4 个单元测试）。

这个文件做了几件别家解析器没做的事，原因写在下面第六节：

- `.meta` 是**整体 JSON**（pretty-printed，几百行），不是 JSONL，所以走
  `fs::read_to_string` + `serde_json::from_str`，不能用 `for_each_json_line`
- 按 `turn_id` 去兄弟 `.jsonl` 里 join 出真实时间戳
- 旧格式轮次（无 `model_usage`）用 `used_tokens` 兜底，标为 `unknown` 而不是丢弃
- 工作区从 `.meta` 的 `working_dir` 字段直接取，而不是从 hash 目录名反推

### 改动 2：注册模块 `crates/tokscale-core/src/sessions/mod.rs`

按字母序插入（当前是 `pi` 之后、`reasonix` 之前那一段，第 49 行附近）：

```rust
pub mod pi;
pub mod prime_agent;
pub mod qwen;
pub mod reasonix;
```

`atomcode` 字母序最靠前，加在列表开头：

```rust
pub mod atomcode;   // ← 新增
pub mod pi;
pub mod prime_agent;
```

### 改动 3：注册客户端 `crates/tokscale-core/src/clients.rs`

在 `Unsloth = 51` 之后追加。**注意给 Unsloth 末尾补一个逗号** —— 它是宏表最后一项，
原本没有逗号（第 1029–1042 行）：

```rust
    Unsloth = 51 => {
        id: "unsloth",
        display: "Unsloth",
        logo: Some("https://github.com/unslothai.png"),
        root: PathRoot::EnvVar {
            var: "UNSLOTH_STUDIO_HOME",
            fallback_relative: ".unsloth/studio",
        },
        relative: "studio.db",
        pattern: "studio.db",
        headless: false,
        parse_local: true,
        submit_default: true
    },                                    // ← 原来是 }，改成 },
    AtomCode = 52 => {                    // ← 新增，序号取 52（当前最大 51）
        id: "atomcode",
        display: "AtomCode",
        logo: Some("https://github.com/AtomGit.png"),
        root: PathRoot::Home,
        relative: ".atomcode/sessions",
        pattern: "*.meta",
        headless: false,
        parse_local: true,
        submit_default: true
    }
);
```

几个字段的说明：

| 字段 | 值 | 为什么 |
|---|---|---|
| `root` | `PathRoot::Home` | 数据在 `~/.atomcode/`，跟 `Qwen`/`Kimi` 一样 |
| `relative` | `.atomcode/sessions` | 会话目录。真实结构是 `sessions/<项目hash>/*.meta` 两层 |
| `pattern` | `*.meta` | 只匹配 meta。**不要**用 `*.jsonl` —— 它没有模型名，只有汇总数 |
| `submit_default` | `true` | 默认参与扫描，用户无需手动勾选 |

关于 `relative` 的两层结构：scanner 是递归的，所以不用写 `<hash>` 那层 ——
`Qwen` 也是同样的写法（`relative: ".qwen/projects"` + `pattern: "*.jsonl"`，
实际文件在 `projects/<project>/chats/*.jsonl` 两层深）。

### 改动 4：接线分发 `crates/tokscale-core/src/lib.rs`

这是最容易漏的一步，而且**有两条通道，两处都要加**。

`lib.rs` 有 650KB，别去数行号，直接搜 `ClientId::Qwen` 定位，然后照抄它的结构。

#### 通道 1：带缓存的增量扫描（第 2747–2755 行附近）

```rust
    // Parse Qwen files
    parse_cached_lane(
        &scan_result,
        &mut source_cache,
        pricing,
        &mut all_messages,
        ClientId::Qwen,
        sessions::qwen::parse_qwen_file,
    );
```

紧跟着加一段：

```rust
    // Parse AtomCode files
    parse_cached_lane(
        &scan_result,
        &mut source_cache,
        pricing,
        &mut all_messages,
        ClientId::AtomCode,
        sessions::atomcode::parse_atomcode_file,
    );
```

#### 通道 2：全量扫描（第 5436–5449 行附近）

```rust
    // Parse Qwen JSONL files in parallel
    let qwen_msgs: Vec<ParsedMessage> = scan_result
        .get(ClientId::Qwen)
        .par_iter()
        .flat_map(|path| {
            sessions::qwen::parse_qwen_file(path)
                .into_iter()
                .map(|msg| unified_to_parsed(&msg))
                .collect::<Vec<_>>()
        })
        .collect();
    let qwen_count = qwen_msgs.len() as i32;
    counts.set(ClientId::Qwen, qwen_count);
    messages.extend(qwen_msgs);
```

照抄一份：

```rust
    // Parse AtomCode session meta files in parallel
    let atomcode_msgs: Vec<ParsedMessage> = scan_result
        .get(ClientId::AtomCode)
        .par_iter()
        .flat_map(|path| {
            sessions::atomcode::parse_atomcode_file(path)
                .into_iter()
                .map(|msg| unified_to_parsed(&msg))
                .collect::<Vec<_>>()
        })
        .collect();
    let atomcode_count = atomcode_msgs.len() as i32;
    counts.set(ClientId::AtomCode, atomcode_count);
    messages.extend(atomcode_msgs);
```

**为什么两处都要加**：这两条是互斥的扫描路径 —— `parse_cached_lane` 走
`SourceMessageCache`（增量，未改动的文件直接读缓存），另一处是全量重扫。
只加一处的话，另一个路径下 AtomCode 会静默消失。

顺带一提：`counts.set` 在 lib.rs 里只有 49 处，而枚举有 52 个客户端 ——
说明确实有部分客户端只注册了单条通道，属于历史遗留。别学它们，**两处都加最稳**。

---

## 四、编译与验证

```bash
# 单元测试（解析器自带 4 个用例，必须全过）
cargo test -p tokscale-core atomcode

# 本地编译
cargo build -p tokscale-core

# 冒烟：拿你本机的真实数据跑一遍
cargo run -p tokscale-cli -- --client atomcode
```

应该能看到类似这样的输出（数字取自你本机 8 月底的真实数据）：

| 模型 | 轮次 | 输入 | 输出 |
|---|---|---|---|
| deepseek-v4-flash | 376 | 12.71M | 2.22M |
| GLM-5.2 | 339 | 11.97M | 800.0K |
| LongCat-2.0 | 178 | 3.53M | 747.4K |
| qwen3.8-27b | 80 | 4.79M | 875.5K |

---

## 五、token-monitor 侧：2 处改动

### 改动 5：加入默认扫描列表 `src/shared/clientTracking.js`

第 9 行的 `DEFAULT_CLIENTS` 是默认开启扫描的客户端 CSV：

```js
const DEFAULT_CLIENTS = 'claude,codex,opencode,hermes,openclaw,cursor,antigravity,cline,kimi,qwen,grok,copilot,pi,zed,kilocode,commandcode,zcode,kiro,codebuddy,workbuddy,proma,reasonix,dsh,cherrystudio,lmstudio';
```

在末尾加上 `atomcode`：

```js
const DEFAULT_CLIENTS = '...,cherrystudio,lmstudio,atomcode';
```

> 如果你只想让它可勾选但不默认开启，则加进 `KNOWN_CLIENTS` 而不是 `DEFAULT_CLIENTS`
> （参考 `micode` 的注释：它因为会重复导入 Claude 会话而故意不默认开启）。
> AtomCode 没有这个问题，直接进默认列表即可。

### 改动 6（仅路线 B）：指向你的 fork 二进制 `scripts/vendor/tokscale.json`

```json
{
  "mode": "override",              // upstream → override
  "fork": "<你的GitHub用户名>/tokscale",
  "commit": "<你的commit sha>",
  "commitTitle": "feat(sessions): add AtomCode as a local session source",
  "baseVersion": "4.15.0",         // 必须与 package.json 里的依赖版本一致
  "releaseRepo": "<你的GitHub用户名>/tokscale",
  "releaseTag": "token-monitor-<你的commit sha 前7位>",
  "platforms": { ... 9 个平台的 sha256 ... }
}
```

`mode` 改 `override` 后，`scripts/ensure-vendored-tokscale.js` 会在安装时
下载你的 release 资产替换掉 npm 安装的那份二进制。

注意两个校验脚本会卡你：
- `verify-vendored-tokscale-clients.js` —— 校验二进制确实支持 `DEFAULT_CLIENTS` 里的每一个 id
- `verify-vendored-tokscale-release.js` —— 校验 `baseVersion` 与 npm 依赖一致、sha256 匹配

所以 `baseVersion` 和每个平台的 sha256 都必须真实，糊弄不过去。

### 图标（可选）

托盘图标按 id 自动找 `assets/icons/atomcode.svg`（见
`src/electron/renderer/trayProviderIcons.js` 第 25 行的 fallback）。
不放文件也不会崩，只是显示占位。要放的话，顺便在
`.github/assets/tools-icon/` 下也放一份。

---

## 六、AtomCode 的数据格式：为什么要这么写

这是本文最有价值的部分。你本机的 `~/.atomcode/` 结构如下：

```
~/.atomcode/
├── codingplan_sync.json              额度同步（免费额度剩余）
├── datalog/                          日志，无 token 数据
├── logs/                             运行日志，无 token 数据
└── sessions/
    └── <项目hash>/                    ← 如 bdec0826c63b4a55
        ├── <uuid>.meta               ← token 数据在这（整体 JSON）
        ├── <uuid>.jsonl              ← 精确时间戳在这
        └── <uuid>.snapshot           ← 只有对话正文，几十 MB，零 token 数据
```

### 坑 1：数据在 `.meta`，不在 `.jsonl`

绝大多数监控器（包括 tokscale 现有全部 52 个客户端）都假设"会话数据 = JSONL 逐行"。
AtomCode 不是 —— `.meta` 是 pretty-printed 的整体 JSON，一个文件几百行：

```
total chars 21417, lines 618
整体 json.loads 成功: True
```

用 `for_each_json_line` 去读会全部解析失败，静默返回空结果 —— **这是最难排查的一类 bug，
因为不报错，只是数字是 0**。

### 坑 2：`.snapshot` 是陷阱

同目录下 `.snapshot` 文件动辄几十 MB，名字看起来最像"会话数据"，
但它**一个 token 字段都没有**。别把 pattern 设成 `*` 或 `*.snapshot`。

### 坑 3：两个文件各持一半信息

| 文件 | 有模型名 | 有 input/output/cached | 有逐轮时间戳 | 有 pricing |
|---|---|---|---|---|
| `.meta` | ✅ | ✅ | ❌ | ✅ |
| `.jsonl` | ❌ | ✅（仅汇总） | ✅ | ❌ |

`.meta` 的 `turn_stats[]` 字段实测：

```
['after_message', 'position_valid', 'turn_id', 'round_count', 'tool_call_count',
 'duration_ms', 'total_tokens', 'errored', 'used_tokens', 'ctx_window', 'model_usage']
```

**注意：没有时间戳字段。** 只有会话级的 `created_at` / `updated_at`。

如果只用 `.meta`，一个跑了三天的长会话，所有轮次都会被盖上 `updated_at` 这一个时间戳，
于是三天的用量全算到最后一天 —— 按天趋势图会严重失真。

`.jsonl` 每行有 `ts`（epoch 毫秒）和 `turn_id`，所以按 `turn_id` join 就能拿到真实时间。
这个 join 的效果很实在：你本机 8/22 那天，按会话更新时间算是 21 轮，
校准后是 9 轮，差的 12 轮回到了它们真实发生的日期。

### 坑 4：`cached_input` 是独立计数，不是 `input` 的子集

```json
"tokens": { "input": 64080, "output": 22518, "cached_input": 331776 }
```

`cached_input` (331776) 远大于 `input` (64080)，显然不是包含关系。
所以 `cache_read` 直接映射 `cached_input`，并且**累加**是安全的。

（顺带：331776 也大于 `ctx_window` 262144，说明它是该轮内多次 API 调用的缓存累计值，符合预期。）

### 坑 5：约一半轮次是旧格式

你本机 1912 个轮次里，939 个没有 `model_usage`，只有 `used_tokens`：

```
turns with model_usage: 973   without: 939
```

这些是 7 月的老会话。丢掉它们会少算 17.67M token（约占总量的 1.7%，
以及几乎全部 7 月的历史）。解析器里把这些归于 `model_id = "unknown"`、
全数计入 `input`，保证总量不丢。

### 坑 6：`.meta` 和 `.jsonl` 的数字对不上（正常）

同一个 turn，两边数字不一致：meta 的 `model_usage` 按**每次模型调用**累计
（缓存部分重复计），jsonl 的 `usage` 按**每轮对话**汇总。

解析器统一走 meta 口径，并且只用 meta —— 因为只有它带厂商自己的 `pricing`，
成本才算得准。jsonl 只用来取时间戳，不取数字，避免两套口径打架。

### 坑 7：当前 `.meta`（`"v": 1`）同时带 `total_tokens` 和 `model_usage`，路由别被 `total_tokens` 骗走

实测中大量 `.meta` 文件**既**有每轮 `total_tokens`（聚合值），**又**有
`model_usage[].tokens{ input, output, cached_input }`（逐次调用明细）。而且
`total_tokens` **不是** 明细的求和——例如某轮 `total_tokens = 36857`，而
`input(44206) + output(1272) + cached_input(170240) ≈ 244k`，差近 6 倍。

因此解析路由必须先看"有没有 `model_usage` 明细"：
- **有 `model_usage`**（哪怕同时有 `total_tokens`）→ 走明细路径，按
  `input/output/cached_input` 拆分，`cache_read` 映射 `cached_input`。
  这一步把缓存命中带出来；漏了就会被 `parse_json_new` 当成纯聚合格式，
  `cached_input` 整个丢掉，仪表盘上"缓存读 / 缓存命中率"全为 0。
- **无 `model_usage`**（旧格式只有 `used_tokens`，或真·新 `.json` 会话只有
  `messages` + 聚合 `total_tokens`）→ 才走 `parse_json_new` 聚合路径。

绝对不要用 `total_tokens` 去回填 `input_tokens`：它和明细口径不一致，会
把输入 token 算少 6 倍，成本与趋势全失真。

---

## 七、改完后的验证清单

- [ ] `cargo test -p tokscale-core atomcode` —— 4 个测试全过
- [ ] `tokscale --help` 的 `--client` possible values 里出现 `atomcode`
- [ ] `tokscale --client atomcode` 能跑出你本机的真实用量，且模型名是
      `deepseek-v4-flash` / `GLM-5.2` / `LongCat-2.0` / `qwen3.8-27b`
- [ ] 按天趋势不再出现"某一天数字异常高、前一天是 0"的现象（坑 3 已修）
- [ ] 7 月的老会话有数据显示，model 列为 `unknown`（坑 5 已兜底）
- [ ] 总额与本脚本 `atomcode_usage.py` 的结果对齐（合计约 1.06B token）

---

## 八、如果你不想碰 Rust

直接用 `atomcode_usage.py`（纯标准库，无需依赖，数据不出本机）：

```bash
python atomcode_usage.py --all          # 全部维度
python atomcode_usage.py --today        # 今天
python atomcode_usage.py --days 7       # 最近 7 天
python atomcode_usage.py --watch 60     # 每 60 秒刷新，可当常驻面板
python atomcode_usage.py --csv out.csv  # 导出明细
python atomcode_usage.py --json out.json
```

它已经处理了上面全部 6 个坑，还额外做了：跨天会话的时间戳校准、
上下文占用率（22.1%）、按项目/模型/天三个维度的拆分。
