# CodeArts Doer 集成分析（token-monitor）

> 日期：2026-09-01 · 前置文档：`codeartsdoer_Token本地统计解析方案.md`（数据源盘点）
> 本文回答：要把 CodeArts Doer 统计集成进本项目，项目侧的结构是怎样的、该套哪个模板、改哪些文件、卡点在哪。
> 所有落点均已在 v0.50.0 源码中逐一核实。

## 1. 结论

- **接入形态：新增一个 limits provider（`codeartsdoer`），模板 = WorkBuddy**（本地 app 会话 + 鉴权 HTTP 端点 + URL 白名单 + `source: 'local'`）。这是项目里唯一同构的先例。
- **token 用量 client 暂不可行**：前文已全量扫描证实本地无 token 落盘（258MB 目录里没有任何 usage 数据），`PARSE_LOCAL_CLIENTS` 路线没有数据源。唯一可能产生按会话用量的是本地 webserver 的 opencode 消息 API（消息体带 tokens），但它的进程依赖（IDE 开着才在）+ 鉴权未解，只能作为**二期**，且要以一期的探测结论为前提。
- **关键前置（gate）：33586 端点的 401 鉴权**。它决定一切：
  - 拿到本地 webserver 鉴权 → 可做账号配额 + 按会话用量（完整方案）；
  - 拿不到但能复用 IDE 的华为云登录态 → 只能做账号配额（credits 窗口，workbuddy 同款）；
  - 两者都不行 → 无集成可做。
  **先做 Phase 0 探测脚本，再决定集成范围。**

## 2. 项目结构（集成相关部分）

### 2.1 Limits provider 子系统（codeartsdoer 的主战场）

调用链：`LimitsRuntime`（`limitsRuntime.js`，bounded 并发 + latest-wins 串行 lane + 重试退避）→ `limitCollector.js`：

```
collectLimitsOnce(options)
  └─ 对 options.limitProviders 里每个选中的 provider
       └─ probeLimitProvider(provider, options, {}, deps)
            ├─ fetcher = providerFetchers(deps)[provider]      // L3952：id → 函数 注册表
            ├─ fetch = createProbeFetch(resolveProviderFetch(provider, deps), …)
            │         // L4029：deps.fetch 注入优先；workbuddy → deps.workbuddyFetch（特例）；
            │         //         grok → grokLimits.resolveGrokFetch；其余 → 全局 fetch
            ├─ 成功 → fetchXxxLimits(options, probeDeps) 返回 provider 记录（或多账号数组）
            └─ 异常 → statusProvider(provider, providerStatusFromError(err))
                     // 错误 → status 映射：ENOENT→notConfigured，带 status 字段透传，其余 unavailable
```

**注册三处（缺一不可）**：
1. `limitProviders.js` `LIMIT_PROVIDER_IDS`——frozen 数组，**兼作新安装默认顺序**；
2. `limitCollector.js` `providerFetchers()`——id → `fetchXxxLimits`；
3. 若 transport 不走注入的 `deps.fetch`（自建鉴权头），在 `resolveProviderFetch()` 加特例分支（workbuddy 是现成样例，L4031）。

**wire 形状**（`limits.js` `normalizeLimitProvider`，L421——local 采集与 hub ingest 的唯一漏斗）：

```
{ provider, accountKey, accountLabel, planLabel, accountName, accountEmail,
  status: ok|disabled|notConfigured|unauthorized|rateLimited|sourceRateLimited|unavailable|error,
  source: oauth|cli|web|rpc|local|api,        // ← codeartsdoer 用 'local'（IDE 插件本地能力）
  updatedAt,
  windows: [{ kind: session|daily|weekly|billing,
              metric: credits|spend,           // 余额型配额必须标 credits
              source: web|local, label, used, limit, remaining, usedPercent, currency, resetsAt }],
  balanceUsd?, balance?: { amount, currency } }
```

状态语义对本集成的具体含义：IDE 未安装/插件未装 → `notConfigured`（对应"探测不到 webserver 进程"）；进程在但鉴权失败 → `unauthorized`；进程不在（IDE 关着）→ 取决于设计，倾向 `unavailable`（区分于"没装"）。

### 2.2 WorkBuddy 模板全链路（逐文件）

| 层 | 文件 | 做什么 | codeartsdoer 对应物 |
|---|---|---|---|
| 解析 | `src/shared/workbuddyLimits.js` | URL 白名单（精确 host+path、只允许 POST、禁 query/port/fragment）、请求头注入、个人/企业分支、`accountKey = hashKey(...)`、错误→status | `src/shared/codeartsdoerLimits.js`（白名单 = `127.0.0.1:33586` 的 opencode 端点，或华为云配额端点） |
| 本地会话 | `src/electron/workbuddyLocalAuth.js` | 每次请求现读 app 凭据文件、前后双校验会话指纹、剥离受保护头、`redirect: 'error'`（认证头绝不跟随重定向） | `src/electron/codeartsdoerLocalAuth.js`（凭据来源 = Phase 0 探测结果，**未定**） |
| 主进程接线 | `src/electron/main.js` | `createWorkbuddyLocalAuth({fetch: electronLimitsFetch()})`（L51）；`electronLimitsConfig()` 平台门控（L680，darwin/win32 only）；`electronLimitsDeps()` 提供 `workbuddyFetch` 适配器（L742，把本地 auth 的响应包成 `{status, ok, json}`） | 同构三处 |
| 配置映射 | `src/electron/runtimeConfig.js` | `CREDENTIAL_SETTING_PATHS`（L87）+ env 映射（headless token 字段）+ `workbuddyDesktopSessionOnly`（桌面模式禁用裸 token 防绕过） | headless 走 env（`TOKEN_MONITOR_CODEARTS*`），桌面走本地会话 |
| 调度 | `limitCollector.js` | `providerFetchers` + `resolveProviderFetch` 特例 | 加两行 |
| 渲染 | `src/electron/renderer/app.js` `LIMIT_PROVIDERS` 数组 + `limitProviderPresentation.js`（capability tag / source label / status label） | 勾选框自动生成 | 同 workbuddy：`['Auto','Desktop app']`、source 'Local'、notConfigured→'Sign in' |
| 测试 | `limitProviderOrder.test.js`（deepEqual 钉死列表+顺序）、`limitProviderPresentation.test.js`、provider 单测 | | 同 |

### 2.3 出站传输（不需要新代码）

- widget：`createElectronLimitsFetch`（`limitsFetch.js`）——配了代理 env 走 `outboundFetch`（undici EnvHttpProxyAgent），否则 Electron `net.fetch`（`credentials: 'omit'` + `cache: 'no-store'` 强制，防 cookie jar 顶掉显式 Cookie、防缓存答配额）。
- **对 localhost 的 webserver 请求同样适用**：`resolveProviderFetch` 的 workbuddy 特例模式已经覆盖"自带鉴权的 transport"——codeartsdoer 照抄即可，代理/cookie 问题被 runtime 边界统一兜住。
- agent/hub：全局 `fetch`，无特殊需求。

### 2.4 Worker 同步（容易漏的硬门禁）

`worker/src/shared/` 里有 **`limitProviders.js` 和 `limits.js` 的 vendored 副本**（`npm run sync:worker` 生成，`@generated`，CI 查漂移）。hub/worker 的 ingest 用 `normalizeProviderId` 校验——**新 provider id 不注册进 `LIMIT_PROVIDER_IDS`，hub 会把 codeartsdoer 的 limits 行整个丢掉**。改完 `src/shared/` 必须跑 `npm run sync:worker`。

### 2.5 Token 用量 client 子系统（二期才碰）

`DEFAULT_CLIENTS`/`PARSE_LOCAL_CLIENTS`（clientTracking.js）→ `clientSourceRoots()`（collector.js，watch 根）→ tokscale 扫描或本地解析器（promaUsage 模板）→ `extractUsageFromTokscale`。若走 webserver 消息 API 路线，它不是"解析本地文件"而是"调本地 HTTP 再映射成 tokscale 形状 entry"——没有现成先例（proma 读文件、qodercn 读 SQLite），需要在 `collectUsageOnce` 加一个类似 workbuddy-fetch 的 HTTP 分支，**且必须处理进程不在（IDE 关着）时的降级**。这是二期范围，且依赖一期结论。

## 3. CodeArts Doer 数据源选项（决定集成范围）

| 选项 | 数据 | 鉴权来源 | 可得性 | 集成形态 |
|---|---|---|---|---|
| A：本地 webserver `127.0.0.1:33586`（opencode 服务） | 账号配额 + **按会话/消息 tokens**（opencode 消息体带 `tokens.{input,output,reasoning,cache.*}`，见本项目 `opencodeSession.js` 对官方 opencode.db 的同构解析） | **未知（401）**——需从插件↔webserver 握手/IDE 登录态里找 | 已确认服务在跑（pid 14468，端口活跃）；token 来源待逆向 | 配额 → limits provider；用量 → 二期 client |
| B：华为云配额 API（插件自身在调，jar 内 `TokenUsageUtil`：`getTokenUsageUrl` / `package_token_amount` / `buildModelQuotaList`） | 账号级套餐配额 | IDE 的华为云登录凭据（位置待定位，疑在 IDEA 登录态/keychain） | 端点与字段已从 jar 字符串确认 | 仅 limits provider（credits 窗口） |
| C：本地文件 | —— | —— | **已证伪**（全量扫描零命中；用量在 JVM 内存/云端） | 不可行 |

**探测点已确认**：`~/.codeartsdoer/webserver_idea_IU-<build>_<vendor>_<ver>.properties` 里有 `pid=` + `port=`（按 IDE 版本分目录）——这是发现"webserver 活着吗、在哪个端口"的稳定入口，集成代码应从这里探测而非硬编码 33586。

## 4. 集成触点清单（一期：limits provider）

| # | 文件 | 改动 |
|---|---|---|
| 1 | `src/shared/codeartsdoerLimits.js` | 新文件（仿 workbuddyLimits.js）：端点白名单、请求、解析、`hashKey('codeartsdoer', identity)`、错误→status 映射 |
| 2 | `src/electron/codeartsdoerLocalAuth.js` | 新文件（仿 workbuddyLocalAuth.js）：本地会话读取 + 指纹校验 + 头注入；**凭据来源 = Phase 0 输出** |
| 3 | `src/shared/limitProviders.js` | `LIMIT_PROVIDER_IDS` 加 `'codeartsdoer'`（放 `thirdparty` 前；新安装默认序变化 = 兼容性变更，需说明） |
| 4 | `src/shared/limitCollector.js` | `providerFetchers` 加注册（1 行）+ `resolveProviderFetch` 加特例（1 行，若走自建鉴权 transport） |
| 5 | `src/electron/main.js` | 三处接线（L51 / L680 / L742 的 workbuddy 同构位）；平台门控（CodeArts 是 Windows/macOS 的 IDEA 插件 → 支持矩阵待确认） |
| 6 | `src/electron/runtimeConfig.js` | `CREDENTIAL_SETTING_PATHS` + headless env 字段（`TOKEN_MONITOR_CODEARTS_*`）+ desktop-session-only 门控 |
| 7 | renderer `app.js` + `limitProviderPresentation.js` | `LIMIT_PROVIDERS` 数组加行；capability/source/status 三个展示映射（仿 workbuddy 的 'Auto'/'Desktop app'/'Local'/'Sign in'） |
| 8 | `docs/API.md` | `limits.providers[].provider` 枚举加 `codeartsdoer`（L308）+ `source: 'local'` 语义说明（L310 的 WorkBuddy 段落旁） |
| 9 | `README.md` ×5 语言 + `.env.example` | Supported Tools 表加行（`— | ✅ | —`，limits-only 形态，同 OpenRouter 行）；正文计数同步（readmeConsistency 测试）；env 示例 |
| 10 | 测试 | `limitProviderOrder.test.js` deepEqual 列表加 id；新 `tests/shared/codeartsdoerLimits.test.js`（fixture 化响应）；`limitProviderPresentation.test.js` 映射 |
| 11 | `npm run sync:worker` | **必须**——worker 的 `limitProviders.js`/`limits.js` 副本漂移会被 CI 打回，且不更新的话 hub 会丢 codeartsdoer 行 |

**不做**（一期）：token 用量 client、session detail、tray 专属图标（可后补）、WSL（IDEA 无 WSL 形态）。

## 5. Phase 0 —— 探测（先于一切编码）

独立脚本（不进仓库），回答三个问题：

1. **webserver 鉴权从哪来**：抓插件进程启动参数/环境变量；对比 `cache.properties`、IDEA 的 `idea.key` 登录态、webserver exe 的启动命令行（`wmic process where processid=14468 get commandline`）；试常见 opencode 鉴权头（`Authorization: Bearer …`、`x-auth-token`）；
2. **端点枚举**：拿到鉴权后打 `/session`、`/session/:id`、`/message`、`/global/health`、`/config`，确认响应体里 usage/quota 字段的真实形状（对照 `opencodeSession.js` 已知的 opencode 数据模型）；
3. **华为云配额端点**（备选 B）：从 `AgentKernel.jar`/`CodeChat.jar` 的字符串里定位 `getTokenUsageUrl` 的实际 URL 与鉴权要求。

判定：
- 1+2 成功 → 一期做 A（配额窗口），二期做用量 client（webserver 消息 → tokscale 形状）；
- 仅 3 成功 → 一期做 B（仅 credits 窗口，无用量）；
- 全失败 → 不集成，关闭 issue，把探测证据归档。

## 6. 风险

| 风险 | 对策 |
|---|---|
| 鉴权 token 生命周期短/与 IDE 登录态强耦合 | 仿 workbuddy 的"每次请求现读 + 前后指纹校验"，不缓存裸 token；过期 → `unauthorized` 状态而非静默 |
| webserver 随 IDE 开合，端口随 IDE 版本变（`webserver_idea_IU-<build>...`） | 端口/进程一律从 properties 文件动态探测；进程不在 → `notConfigured`/`unavailable` 分状态处理 |
| opencode 上游 API 变更（CodeArts 基于 opencode 但版本锁定 `26.8.202`） | 解析器防御性（多字段别名），版本漂移时走 `unavailable` 不炸 tick |
| `LIMIT_PROVIDER_IDS` 顺序 = 新安装默认序（兼容面） | 新 id 追加在 `thirdparty` 前，不动既有顺序；PR 描述里说明 |
| 多 IDE 版本并存（多个 webserver 目录） | properties 文件按 build 分目录，取 pid 存活且端口可达的那个 |

## 7. 一句话

项目侧的集成模板完全现成（WorkBuddy 五层接线 + worker 同步门禁 + 展示映射），codeartsdoer 的唯一不确定性在**数据源鉴权**——先跑 Phase 0 探测，再按 §3 的判定表决定一期做 A 还是 B，工程部分照 §4 清单落。
