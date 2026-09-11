# AtomCode 支持：编译与测试指南

本文回答一个问题：**Rust 解析器（`tmp/tokscale`）已经写完，如何编译、测试、验证它真的能工作。**

> 适用：本机 Windows（Git Bash），tokscale 源码已克隆到 `tmp/tokscale`（v4.15.0），
> `atomcode.rs` / `clients.rs` / `mod.rs` / `lib.rs` 四处已改完（盲写，未经编译）。

---

## 0. 先读：一条必须理解的边界

你在 `tmp/tokscale` 里改的是 **tokscale 的 Rust 源码**。但 token-monitor 运行时
**不会去编译这份源码**——它 spawn 的是一个**预编译的 tokscale 二进制**
（Windows x64 下来自 npm 包 `@tokscale/cli-win32-x64-msvc`，或 `tokscale` shim），那个二进制**不含你的改动**。

所以"运行 + 编译测试"分成两件事，目标不同：

| 阶段 | 验证什么 | 碰不碰 token-monitor | 依赖 |
|---|---|---|---|
| **阶段 1** 编译 | Rust 代码能否 `cargo build` 通过 | 否 | 仅 Rust 工具链 |
| **阶段 2** 单元测试 | 解析器对 `.meta`/`.jsonl` 的解析逻辑对不对 | 否 | Rust + 4 个内联测试 |
| **阶段 3** 冒烟 | 对**真实** `~/.atomcode/sessions` 数据能否解析出 token | 否（直接用编译产物） | 编译产物 + 本机真实数据 |
| **阶段 4** 端到端 | 编译产物接入 token-monitor，采集链路是否认得 atomcode | **是** | 阶段 1 + `npm install` |
| **阶段 5** JS 回归 | token-monitor 侧 JS 触点（`clientTracking` / `usage` 等）接对没有 | 是（纯 JS 单测） | Node ≥ 22.15 |

**最快闭环 = 阶段 1 → 2 → 3**：三条命令就能确认"我的 Rust 改动编译得过、单测过、
能解析真实数据"，完全绕开 Electron 启动、npm 包缺失这些坑。
阶段 4/5 是"接入 + 回归"，在你确认阶段 3 通过后再做。

> ⚠️ 一个反复出现的陷阱：**你的改动在源码里，token-monitor 跑的是二进制。**
> `scripts/vendor/tokscale.json` 当前是 `mode: "upstream"`——`ensure:tokscale` 是 **no-op**
> （不下载、不替换），所以它**不会**帮你接入本地构建，也**不会**覆盖你放进去的产物。
> 要让 token-monitor 用你的构建，只能**手动把产物放到它查找二进制的位置**（阶段 4 讲怎么做）。
> `npm install` 会把 node_modules 还原成纯 npm 版二进制——所以"换进去的产物"是易失的，
> 重装依赖后需要再放一次。

---

## 阶段 0 · 准备 Rust 工具链（一次性）

token-monitor 与 tokscale 的 JS 部分**不依赖** Rust，所以只要不跑 cargo，
JS 测试照常。只有阶段 1/2/3 需要 Rust。

```bash
# 1) 确认是否已装
cargo --version && rustc --version
```

若 `not found`，用 rustup 安装（stable，MSVC 工具链）：

```bash
# Windows: 用 winget / 官网 rustup-init.exe 安装 rustup
winget install -e --id Rustlang.Rustup
# 装完重开终端，或
# https://www.rust-lang.org/tools/install  下载 rustup-init.exe 双击
rustup default stable
```

> 说明：tokscale 工作区**没有** `rust-toolchain.toml` 钉版本，用系统 stable 即可。
> 若 winget / 网络反复超时，手动下载 `rustup-init.exe` 离线安装更稳。

装完确认：

```bash
cargo --version    # 期望: cargo 1.7x.x
rustc --version    # 期望: rustc 1.7x.x
```

---

## 阶段 1 · 编译 tokscale 源码

所有 cargo 命令都在 **`tmp/tokscale`** 目录下跑（那是 Cargo 工作区根）。

```bash
cd E:/project/token-monitor/tmp/tokscale

# 编译 release（产物: target/release/tokscale.exe）
cargo build --release
```

首次编译会拉取并编译全部依赖（tokscale-core / tokscale-cli / 第三方 crate），
**可能 3–10 分钟**，属正常。

**成功的标志**（最后几行）：

```
   Compiling tokscale-core v4.15.0 (...)
   Compiling tokscale-cli v4.15.0 (...)
    Finished `release` profile [optimized] target(s) in XXXs
```

**产物位置**：

```
tmp/tokscale/target/release/tokscale.exe
```

### 这一阶段最可能卡住的地方（盲写代码，预期会有一批编译错误）

atomcode 相关源码是**静态逐字段核对**的，没经过编译器。第一遍 `cargo build`
大概率报错，集中在 `crates/tokscale-core/src/sessions/atomcode.rs` 与 `lib.rs` 两处分发点。
常见类型：

| 报错样子 | 原因 | 怎么修 |
|---|---|---|
| `cannot find function parse_atomcode_file` | `mod.rs` 未 `pub mod atomcode;` 或函数可见性 | 确认 `pub mod atomcode;`（已在 `sessions/mod.rs:8`）；函数须 `pub` |
| `no variant named AtomCode` | `clients.rs` 的 `ClientId::AtomCode` 未生效 / 枚举派生宏没覆盖 | 确认 `AtomCode = 51 => {...}` 在 `clients.rs` 且派生宏（`derive` 行）支持新变体 |
| `mismatched types` / `expected ParsedMessage, found ...` | 解析器返回类型与 `unified_to_parsed`/分发点约定不符 | 对 `lib.rs:2685` 与 `lib.rs:5331` 两处分发点的期望签名 |
| `field cache_read does not exist` | `TokenBreakdown` 字段名记错 | 对 `lib.rs` 中 `TokenBreakdown` 真实字段名 |
| `unused import` / dead_code 警告 | 不影响通过，可先忽略 | 编译能过即可，警告后置 |

**修的方式**：按报错行号打开对应文件，改字段名/类型/可见性，**不要**删逻辑。
改完 `cargo build --release` 再来一轮，直到 `Finished`。

> 提示：`cargo build -p tokscale-core` 只编译核心 crate，比全量快，
> 可先用它快速迭代 atomcode.rs 本身的错误，最后再全量 `cargo build --release`。

---

## 阶段 2 · 跑 atomcode 单元测试

解析器自带 4 个测试（`crates/tokscale-core/src/sessions/atomcode.rs` 的 `mod tests`），
用 `tempfile` + 内联字符串构造 `.meta`/`.jsonl`，**不依赖外部 fixture、与 cwd 无关**：

| 测试 | 覆盖什么 |
|---|---|
| `parses_model_usage_with_jsonl_timestamp` | 主路径：meta 取 token/model，**jsonl 的精确时间戳**优先于 `updated_at`；`cached_input` 与 `input` 相加不是子集 |
| `falls_back_to_session_timestamp_without_jsonl` | 没有 jsonl 时回落到 session 时间戳 |
| `legacy_turn_without_model_usage_is_not_dropped` | 老数据无 `model_usage` 时不丢轮次（model 记 `unknown`） |
| `malformed_meta_yields_empty_vec` | 坏 JSON 返回空数组，不 panic |

```bash
cd E:/project/token-monitor/tmp/tokscale

# 只跑 atomcode 的 4 个测试（推荐，最快）
cargo test -p tokscale-core -- sessions::atomcode

# 或跑整个核心 crate 的测试（顺带确认你改 lib.rs 分发点没弄坏别家）
cargo test -p tokscale-core

# 整个工作区（最慢，最终把关用）
cargo test
```

**成功标志**：

```
running 4 tests
test sessions::atomcode::tests::malformed_meta_yields_empty_vec ... ok
test sessions::atomcode::tests::legacy_turn_without_model_usage_is_not_dropped ... ok
test sessions::atomcode::tests::falls_back_to_session_timestamp_without_jsonl ... ok
test sessions::atomcode::tests::parses_model_usage_with_jsonl_timestamp ... ok

test result: ok. 4 passed; 0 failed; 0 ignored
```

**若某个断言失败**：读 `assert_eq!` 打印的 left/right——通常是解析器某字段读错
（token 数值 / 时间戳来源 / model 名）。这恰好证明单测在保护你：按断言改 atomcode.rs，
别改断言。

> 注意：`cargo test` 会另建一份 `target/debug`（默认 debug profile）。
> 若你已 `cargo build --release`，可加 `--release` 复用产物、省编译时间。

---

## 阶段 3 · 冒烟：用编译产物扫真实数据（最快闭环的关键）

这一步**不碰 Electron、不碰 npm 包**，直接拿阶段 1 的产物去扫你本机真实的
`~/.atomcode/sessions`，等价于 collector 真实调用（同 flag）。

**3.1 先确认本机有数据可扫**：

```bash
ls ~/.atomcode/sessions/
# 应有若干 <projectHash>/ 子目录，每个里面是 *.meta / *.jsonl
```

**3.2 跑 collector 同款命令**（`--client` 只扫 atomcode，`--today` 限今天）：

```bash
cd E:/project/token-monitor
BIN=tmp/tokscale/target/release/tokscale.exe

"$BIN" --json --client atomcode --group-by client,session,model --today
```

> Windows 下若 `.exe` 在 Git Bash 里报权限/AV 拦截，改用 `cmd //c` 或 PowerShell：
> `tmp/tokscale/target/release/tokscale.exe --json --client atomcode --group-by client,session,model --today`

**成功标志**：

- 进程退出码 `0`，stdout 是**合法 JSON**（`{ ... }`）。
- JSON 里 `entries` 数组存在 atomcode 行（`client` 字段 = `"atomcode"`）。
- 若你今天跑过 AtomCode，token 数非 0；今天没跑则可能 0 行——换 `--month` 再看：

```bash
"$BIN" --json --client atomcode --group-by client,session,model --month
```

**若编译过但扫出 0 行 / 无 atomcode**：按这个顺序排查——

1. 数据不在预期路径：确认 `~/.atomcode/sessions/<hash>/` 下确有 `.meta` 文件。
2. client id 对不上：解析器输出 `client = "atomcode"`，而 `--client atomcode` 过滤的是
   注册表里的名字——确认 `clients.rs` 里 `AtomCode` 的 `display`/canonical 名是 `atomcode`。
3. 时间窗口没覆盖：`--today` 只算今天，换 `--month` 或去掉日期 flag（默认全量）。
4. 用 `--client atomcode` 不行时，试**不传 `--client`**（全量）看 atomcode 是否出现在
   其它 client 旁边——能出现说明是过滤名的问题，不是解析器的问题。

> 这一步过了，基本可以判定"Rust 侧改动是好的"。剩下的都是接入与回归。

---

## 阶段 4 · 接入 token-monitor 端到端

目标：让 token-monitor 真正用**你的构建**去采集，验证整条链路认得 atomcode。

**4.1 装依赖**（当前 `node_modules/tokscale` 未装，必须先装，否则 collector 直接崩）：

```bash
cd E:/project/token-monitor
npm install
```

装完确认 Windows x64 二进制包到位：

```bash
ls node_modules/@tokscale/cli-win32-x64-msvc/bin/tokscale.exe
```

**4.2 把你的构建换进二进制查找路径**

collector 的 `locateBundledBinary()` 按这个顺序找二进制：
bundled 包 `node_modules/@tokscale/cli-win32-x64-msvc/bin/tokscale.exe` →
downloaded 指针 → `tokscale/bin.js` shim。最省事的是覆盖 bundled 那份：

```bash
cp tmp/tokscale/target/release/tokscale.exe \
   node_modules/@tokscale/cli-win32-x64-msvc/bin/tokscale.exe
```

> `mode: "upstream"` 下 `ensure:tokscale` 不做事，所以这个替换**不会被还原**
> （除非你再跑 `npm install`）。
> 换完自检——直接跑它，应能输出 atomcode 数据（同阶段 3 的命令）：
> `node_modules/@tokscale/cli-win32-x64-msvc/bin/tokscale.exe --json --client atomcode --group-by client,session,model --month`

**4.3 跑一次 dry-run 采集，看 atomcode 有没有数**

`agent:once --dry-run` 会触发 `ensure:tokscale`（upstream 模式 no-op），然后走
**真实采集链路**（collector → 你换进去的二进制 → usage.js 解析），但**不发布**：

```bash
npm run agent:once -- --dry-run
```

**成功标志**：dry-run 日志里能看到 atomcode 的用量条目（token 数与阶段 3 一致）。

> 想更彻底：`npm start` 起 widget，在 GUI 里把 `atomcode` 加入跟踪列表
> （它是 opt-in，不进默认跟踪），看 Home/用量页是否出现 atomcode 一行。
> 注意 opt-in：没手动加跟踪，collector 不会扫它，这是**预期行为**不是 bug。

**4.4 收尾**：验证完若想还原官方二进制，重跑 `npm install` 即可
（node_modules 回到纯 npm 版）。

---

## 阶段 5 · token-monitor 侧 JS 回归

token-monitor 侧 5 处触点（`clientTracking.js` / `usage.js` / `clientHealth.js` /
`collector.js` / 测试）接对没有，用纯 JS 单测确认（**不需要 Rust**）：

```bash
cd E:/project/token-monitor
npm test
```

**与本次 atomcode 直接相关、必须绿**：

```bash
# clientTracking：atomcode 进 KNOWN_CLIENTS / PARSE_LOCAL_CLIENTS，不进 DEFAULT_CLIENTS
node --test tests/shared/clientTracking.test.js
```

**已知的预存红（与本次改动无关，别误判）**：

- `tests/shared/clientHealth.test.js`、`tests/shared/clientPartitionInvariants.test.js`
  —— `Cannot find module 'semver'`，**加载期**就崩，环境问题（依赖未装齐 / 版本不符）。
  先 `npm install` 解决模块缺失，再看是否真失败。
- 三向一致性测试 `share one display order` 在本轮改动前就已红：在途的
  `codeartsdoer` 已进 shared + renderer `KNOWN_CLIENTS`，但**没进任何 README 表格**，
  且 `.github/assets/tools-icon/codeartsdoer.png` 缺失——那是 codeartsdoer 的债，
  不是 atomcode 引入的。atomcode 要让自己的三向一致，README 表格行是**阶段 5 的另一半
  工作**（属于"补齐 JS 触点"，不在本文"编译测试"范围内）。

> 完整验收 = `npm run verify`（`lint` + `test`）。但它依赖上面那些预存红先转绿，
> 所以先用 `node --test <具体文件>` 看局部，再跑整体。

---

## 排错速查

| 现象 | 大概率原因 | 处理 |
|---|---|---|
| `cargo: not found` | 没装 Rust | 阶段 0 装 rustup |
| `cargo build` 报 atomcode.rs 编译错 | 盲写代码字段名/类型没对 | 按报错改 atomcode.rs / lib.rs，别删逻辑 |
| `cargo test` 某断言失败 | 解析器某字段读错 | 按 left/right 改解析器，别改断言 |
| 编译过，扫出 0 行 | 没数据 / client 名不符 / 时间窗没覆盖 | 阶段 3.2 的 4 步排查 |
| collector 崩 `Cannot find module 'tokscale/bin.js'` | `npm install` 没跑 | 阶段 4.1 先 `npm install` |
| 换产物后 `npm run agent:once` 仍是官方行为 | 没覆盖到正确路径 / 又 `npm install` 了 | 确认阶段 4.2 的目标文件，必要时重 cp |
| `npm test` 里 clientHealth / partition 崩 | 预存 `semver` 模块缺失 | 环境问题，`npm install` 后再看 |

---

## 附录：最小命令清单（贴进终端直接跑）

```bash
# === 阶段 0：Rust（一次性）===
cargo --version && rustc --version          # 没有就 winget install Rustlang.Rustup

cd E:/project/token-monitor/tmp/tokscale

# === 阶段 1：编译 ===
cargo build --release
# 产物: tmp/tokscale/target/release/tokscale.exe

# === 阶段 2：单元测试 ===
cargo test -p tokscale-core -- sessions::atomcode    # 期望 4 passed

# === 阶段 3：冒烟（真实数据）===
cd E:/project/token-monitor
tmp/tokscale/target/release/tokscale.exe \
  --json --client atomcode --group-by client,session,model --month

# === 阶段 4：接入 token-monitor ===
npm install                                          # 必须先装（装完会还原官方二进制）
cp tmp/tokscale/target/release/tokscale.exe \
   node_modules/@tokscale/cli-win32-x64-msvc/bin/tokscale.exe   # 再覆盖（顺序别反）
npm run agent:once -- --dry-run                     # 期望日志出现 atomcode 用量

# === 阶段 5：JS 回归 ===
node --test tests/shared/clientTracking.test.js     # 局部
npm test                                            # 整体（注意预存红）
```

> 顺序别跳：1 不过 2/3 没意义；3 过了再花 4/5 的时间接入。
> 全程不碰 `git push`，产物在 node_modules / target 里都是易失的，提交前别依赖它们。
