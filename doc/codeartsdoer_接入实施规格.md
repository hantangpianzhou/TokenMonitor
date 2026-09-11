# CodeArts Doer 接入实施规格

> 日期：2026-09-01 · 版本基线：token-monitor v0.50.0（`origin/main` @ 36307e7）
> 前置文档：`codeartsdoer_Token本地统计解析方案.md`（数据源盘点）、`codeartsdoer_Token统计集成分析.md`（集成触点）
> 本文定位：把集成分析 §4 的 11 个触点落成**可直接编码的规格** —— 精确行号、代码骨架、实测修正。
> 所有行号均在 v0.50.0 源码中逐条核实。

---

## 0. 实测更新（Phase 0 已推进）

本机就是 CodeArts Doer 运行环境，实测结果**推翻了前置文档里"鉴权未知"的判断**：

| 项 | 前置文档结论 | 本次实测 | 证据 |
|---|---|---|---|
| 鉴权方式 | **未知（401）**，猜测 Bearer / `x-auth-token` | **HTTP Basic**，realm=`Secure Area` | `curl -i http://127.0.0.1:33586/` → `WWW-Authenticate: Basic realm="Secure Area"` |
| 探测入口 | `~/.codeartsdoer/webserver_idea_IU-<build>_<vendor>_<ver>.properties` | 确认存在，内容 `pid=14468` / `port=33586` | 文件实读 |
| pid 可靠性 | 未评估 | **pid 准确**；`netstat` 确认 14468 = `LISTENING 127.0.0.1:33586` | `tasklist /FI "PID eq 14468"` → `webserver-26.8.202-1788139078146.exe` |
| 33586 上的 16404 | — | 是 `idea64.exe`（客户端），**不是** webserver | `tasklist /FI "PID eq 16404"` |
| 凭据存放 | 疑在 IDEA 登录态/keychain | `CodeElfPersistentState.xml` 只有 `webserverPort` 一项，**无凭据** | 键名枚举（值脱敏） |
| 端点可达性 | 服务在跑 | 未认证一律 401（`/`、`/health`、`/global/health` 同） | curl |

**结论**：Phase 0 的问题 1（鉴权从哪来）已从"未知"收敛到"**找 Basic 的用户名/密码**"。这是决定性的 —— Basic 是固定凭据而非短时效 token，集成架构可以从"每次请求现读会话 + 指纹校验"简化为"读一次凭据文件 + 缓存"，但仍建议保留 workbuddy 的现读策略（见 §3）。

**仍未定位**：Basic 凭据本体。剩余三条线索见 §8。

---

## 1. 对前置文档的事实修正（先读这节）

集成分析 §2.2 / §2.3 有 5 处与真实代码不符，照它编码会走弯路：

| # | 前置文档的说法 | 实际（v0.50.0 核实） |
|---|---|---|
| 1 | URL 白名单在 `workbuddyLimits.js` | 在 **`src/electron/workbuddyLocalAuth.js:50`** `isAllowedWorkbuddyApiUrl()`。`workbuddyLimits.js` 只做请求构造与响应解析，不含任何白名单 |
| 2 | `resolveProviderFetch` 的 provider 特例（L4031）是"自带鉴权 transport 的覆盖点" | **在 widget 下是死代码**。`electronLimitsDeps()`（main.js:740）恒设置 `fetch: electronLimitsFetch()`，而 `resolveProviderFetch` L4030 先判 `deps.fetch` 就返回了，永远到不了 L4031。真正生效的是 **provider 内部用 `requestDeps` 覆盖 `deps.fetch`**（`workbuddyLimits.js:390-395`） |
| 3 | 白名单"精确 host+path、只允许 POST" | 还硬编码了 **`url.protocol === 'https:'`** 且 **`!url.port`**（`workbuddyLocalAuth.js:54-60`）。对 `127.0.0.1:33586` 的 http 服务**两条都不满足**，必须放宽 |
| 4 | main.js 三处接线 L51 / L680 / L742 | L51 对；门控块实际在 **L681-690**（`electronLimitsConfig()` 内）；L742 对 |
| 5 | `LIMIT_PROVIDER_IDS` 未列实际值 | 当前 **23 个**（`limitProviders.js:5-10`），renderer `LIMIT_PROVIDERS` 在 `app.js:82`（22 项，无 `thirdparty` 差异：实际有） |

### 修正后的真实调用链

```
LimitsRuntime (limitsRuntime.js)   bounded 并发 + latest-wins lane + 重试退避
   └─ collectLimitsOnce (limitCollector.js:4053)
        └─ probeLimitProvider (limitCollector.js:4036)
             ├─ fetcher = providerFetchers(deps)[provider]        ← 注册表 3952
             ├─ probeDeps.fetch = createProbeFetch(resolveProviderFetch(provider, deps), …)
             │      └─ resolveProviderFetch: deps.fetch 存在 → 直接返回它  ⚠ 特例不生效
             └─ fetcher(options, probeDeps)
                    └─ codeartsdoerLimits.fetchCodeArtsDoerLimits(options, probeDeps)
                           └─ requestDeps = { ...deps, fetch: (url, init) =>
                                   deps.codeartsdoerFetch(url, init, expectedSession) }   ← 真正的覆盖点
                                   ↓
                           electronCodeArtsDoerLocalAuth.request()
                                   ├─ isAllowedCodeArtsDoerApiUrl()   ← 白名单（须允许 http + port）
                                   ├─ locateCredentials()             ← 每次请求现读
                                   └─ Authorization: Basic base64(u:p)
```

---

## 2. 数据源与形态（一期范围）

| 选项 | 数据 | 鉴权 | 状态 | 一期 |
|---|---|---|---|---|
| A 本地 webserver `127.0.0.1:33586` | 账号配额 + 按会话 tokens | **Basic（已确认类型，凭据待定位）** | 服务在跑，401 | ✅ 配额 |
| B 华为云配额 API | 账号级套餐 | IDE 华为云登录态 | 端点字段已从 jar 字符串确认 | 备选 |
| C 本地文件 | — | — | 已证伪（全量扫描零命中） | ❌ |

**一期只做 limits provider（账号配额）**；token 用量 client 是二期，见 §7。

---

## 3. 新文件 1：`src/shared/codeartsdoerLimits.js`

逐段对照 `workbuddyLimits.js`（456 行）改写，骨架如下。左侧是 workbuddy 的行号，右侧是 codeartsdoer 该怎么写。

| workbuddyLimits.js | codeartsdoerLimits.js 对应实现 |
|---|---|
| L7-11 常量：endpoint / path / productCode | `CODEARTS_FETCH_TIMEOUT_MS = 8_000`；**端口不硬编码**——由 `codeartsdoerFetch` 侧从 properties 解析后注入，或暴露 `codeartsdoerPort` setting 供覆盖 |
| L22-30 `firstSetting()` | 复用同样的"显式 setting → env → 空"三级回退：`TOKEN_MONITOR_CODEARTS_PORT` / `TOKEN_MONITOR_CODEARTS_USER` / `TOKEN_MONITOR_CODEARTS_PASSWORD` |
| L77-102 `numberOrNull` / `toIso` / `pickValue` | 直接照搬（纯工具，无 provider 语义） |
| L274-294 `httpError` / `applicationError` | **status 映射要改**：`ENOENT`（进程不在/无 properties）→ `notConfigured`；401 → `unauthorized`；ECONNREFUSED → `notConfigured`（IDE 关着 ≠ 鉴权失败） |
| L296-315 `fetchJson` | 照搬 `runWithProbeDeadline` 包裹；注意 `deps.fetch` 会被 requestDeps 覆盖 |
| L340-345 `workbuddyAccountKey` | `hashKey('codeartsdoer', ...)`：有 userId → `user:${userId}`；否则 `port:${port}`（本机单实例，端口即实例标识） |
| L347-437 `fetchWorkbuddyLimits` 主函数 | 见下方骨架 |

```js
'use strict';

const { hashKey } = require('./hashKey');
const { normalizeLimitProvider } = require('./limits');
const { runWithProbeDeadline } = require('./probeDeadline');

const CODEARTS_FETCH_TIMEOUT_MS = 8_000;

async function fetchCodeArtsDoerLimits(options = {}, deps = {}) {
  const env = deps.env || process.env;
  const now = (deps.now || Date.now)();
  const updatedAt = new Date(now).toISOString();

  // 一期：桌面走本地 webserver 会话；headless 走 env 显式凭据。
  const hasExplicit = Boolean(codeartsUser(env, options) && codeartsPassword(env, options));
  const useLocalApp = !hasExplicit
    && options.codeartsdoerDesktopSessionEnabled === true
    && typeof deps.codeartsdoerFetch === 'function';

  const source = {
    provider: 'codeartsdoer',
    source: useLocalApp ? 'local' : 'api',
    sourceDetail: useLocalApp ? 'app' : 'unknown',
    updatedAt,
    windows: []
  };

  if (!hasExplicit && !useLocalApp) {
    return normalizeLimitProvider({ ...source, status: 'notConfigured' });
  }

  // 关键（见 §1 修正 2）：覆盖 deps.fetch，而不是依赖 resolveProviderFetch 特例。
  const expectedSession = useLocalApp ? { port: options.codeartsdoerPort || null } : null;
  const requestDeps = useLocalApp
    ? {
        ...deps,
        fetch: (url, init) => deps.codeartsdoerFetch(url, init, expectedSession)
      }
    : deps;

  const accountKey = hashKey('codeartsdoer', `port:${options.codeartsdoerPort || 'auto'}`);
  try {
    const body = await fetchJson('/<quota-or-usage-path>', {
      method: 'POST',
      redirect: 'error',
      headers: { Accept: 'application/json', 'Content-Type': 'application/json' },
      body: '{}'
    }, requestDeps);
    const usage = parseCodeArtsUsage(body);          // 防御性：多字段别名，见 §6
    return normalizeLimitProvider({
      ...source,
      accountKey,
      accountLabel: usage.planLabel || 'CodeArts Doer',
      status: 'ok',
      windows: usage.windows,
      ...(usage.balance ? { balance: usage.balance } : {})
    });
  } catch (error) {
    return normalizeLimitProvider({ ...source, accountKey, status: codeartsStatus(error) });
  }
}
```

**windows 形状**（过 `limits.js:421 normalizeLimitProvider` 的唯一漏斗）：

```js
// 余额型配额必须标 metric: 'credits'（否则 UI 会当百分比渲染，见 limitBalanceDisplay.js）
{ kind: 'billing',              // session | daily | weekly | billing
  label: 'Credits',
  metric: 'credits',            // 'credits' | 'spend'
  source: 'local',
  used, limit, remaining, usedPercent, currency, resetsAt }
```

---

## 4. 新文件 2：`src/electron/codeartsdoerLocalAuth.js`

仿 `workbuddyLocalAuth.js`（283 行），但有三处**必须不同**：

| 点 | workbuddy | codeartsdoer |
|---|---|---|
| 协议白名单 | `url.protocol === 'https:'`（L54） | **必须允许 `http:`**（localhost 明文） |
| 端口 | `!url.port`（L60，禁端口） | **必须允许端口**，且要与当前探测到的 port 相等 |
| 凭据形态 | `Authorization: Bearer <accessToken>` + 一堆 X-* 头（L244-252） | **`Authorization: Basic base64(user:password)`** |

```js
// 白名单：只放行本机 webserver，且端口必须与探测值一致
function isAllowedCodeArtsDoerApiUrl(value, method = 'GET', port = null) {
  try {
    const url = new URL(String(value || ''));
    return String(method || '').toUpperCase() === 'POST'
      && (url.protocol === 'http:' || url.protocol === 'https:')
      && (url.hostname === '127.0.0.1' || url.hostname === 'localhost' || url.hostname === '[::1]')
      && Number(url.port || 80) === Number(port)     // ← 端口动态校验，不硬编码 33586
      && !url.username && !url.password
      && !url.search && !url.hash;
  } catch (_) {
    return false;
  }
}
```

`request()` 沿用 workbuddy 的四段式防御（L227-261）：

1. `sanitizeRequestInit()` 剥离受保护头（**Basic 的 `authorization` 必须进 `PROTECTED_HEADERS`**，否则调用方可注入任意凭据）；
2. 白名单校验；
3. 现读凭据 + 会话指纹校验（`expectedSession.port` 前后比对，端口变了说明 webserver 重启过 → `unauthorized`）；
4. `redirect: 'error'` —— 认证头绝不跟随重定向。

### 端口发现（不要硬编码 33586）

```
~/.codeartsdoer/webserver_idea_IU-<build>_<vendor>_<ver>.properties
   pid=14468
   port=33586
```

- 按 IDE build 分目录，多版本并存时取 **pid 存活 && 端口可达** 的那个；
- properties 的 `pid` 可能陈旧（IDE 重启后未刷新）→ **以端口 LISTENING 为准，pid 只做辅助**；
- 进程不在（IDE 关着）→ 返回 `notConfigured`，`notConfigured` 与 `unauthorized` 必须区分开。

---

## 5. 注册表与接线（五处，精确行号）

| # | 文件:行 | 改动 |
|---|---|---|
| 1 | `src/shared/limitProviders.js:5-10` | `LIMIT_PROVIDER_IDS` 加 `'codeartsdoer'`。**追加在 `'thirdparty'` 之前**，不动既有顺序（该数组兼作新安装默认序，是兼容面） |
| 2 | `src/shared/limitCollector.js:3952-3977` | 注册表加一行：`codeartsdoer: (o, d) => codeartsdoerLimits.fetchCodeArtsDoerLimits(o, d)` |
| 3 | `src/shared/limitCollector.js:4029` | `resolveProviderFetch` **不需要改**（见 §1 修正 2，改了也不生效）。若坚持加特例，注意 L4030 的 `deps.fetch` 短路 |
| 4 | `src/electron/main.js` | 三处：L51 附近 `createCodeArtsDoerLocalAuth({ fetch: electronLimitsFetch() })`；L681-690 `electronLimitsConfig()` 加 `codeartsdoerDesktopSessionSupported/Enabled` + 平台门控（win32/darwin）；L742 附近 `electronLimitsDeps()` 加 `codeartsdoerFetch` 适配器（把 auth 响应包成 `{status, ok, json}`） |
| 5 | `src/electron/runtimeConfig.js:87` | `CREDENTIAL_SETTING_PATHS` 加 `codeartsdoer: ['codeartsdoerPort', 'codeartsdoerUser', 'codeartsdoerPassword']`；L149-213 照 workbuddy 的 desktop-session-only 门控（桌面禁用裸凭据，防绕过本地会话） |

**渲染层**（`src/electron/renderer/`）：

| 文件:行 | 改动 |
|---|---|
| `app.js:82` | `LIMIT_PROVIDERS` 加 `{ id: 'codeartsdoer', label: 'CodeArts Doer' }` |
| `limitProviderPresentation.js:38` | `codeartsdoer: { local: 'Local', api: 'API' }`（source 标签） |
| `limitProviderPresentation.js:70` | `codeartsdoer: ['Auto', 'Desktop app']`（capability tag） |
| `limitProviderPresentation.js:289` | 加入 `notConfigured → { label: 'Sign in', tone: 'setup' }` 的 provider 名单 |

---

## 6. Worker 同步（硬门禁，漏了 CI 会红）

`worker/src/shared/` 有 `limitProviders.js` 与 `limits.js` 的 **vendored 副本**（`npm run sync:worker` 生成，`@generated`）。hub/worker 的 ingest 用 `normalizeProviderId` 校验 —— **新 id 不注册进 `LIMIT_PROVIDER_IDS`，hub 会把 codeartsdoer 的 limits 行整个丢掉**。

```
改完 src/shared/ → npm run sync:worker → 提交两个目录
```

---

## 7. 测试清单

| 文件 | 内容 |
|---|---|
| `tests/shared/codeartsdoerLimits.test.js` | 新文件（仿 `workbuddyLimits.test.js`）：fixture 化响应 → 断言 windows 形状；401→`unauthorized`；ECONNREFUSED→`notConfigured`；端口漂移→`unauthorized` |
| `limitProviderOrder.test.js` | `deepEqual` 钉死列表 + 顺序，加 `codeartsdoer` |
| `limitProviderPresentation.test.js` | 三个展示映射 |
| `tests/electron/codeartsdoerLocalAuth.test.js` | 白名单：https-only 断言要**反转**（应放行 http）、端口不匹配应拒绝、`authorization` 头必须被 sanitize 剥离 |

---

## 8. Phase 0 剩余项（凭据定位）

Basic 凭据本体未找到，三条待挖线索：

1. **webserver 进程命令行** —— 凭据极可能以启动参数传入（`--user=… --password=…`）。本机 `Get-CimInstance Win32_Process` 未取到输出，需在管理员会话下重试；
2. **jar 内字符串** —— 插件目录 `%APPDATA%\JetBrains\IntelliJIdea2026.1\plugins\CodeArts_Agent_223-253_1787156173762_21e4b363\lib\*.jar`，搜 `Secure Area`、`Basic`、`webserver.password`；
3. **IDEA 日志** —— `idea.log` 中 webserver 启动段落（当前 grep `webserver` 在活跃日志里零命中，需翻 `idea.1.log` ~ `idea.10.log`）。

判定：

| 结果 | 一期范围 |
|---|---|
| 拿到 Basic 凭据 | A：webserver 配额窗口（+ 二期可做按会话用量） |
| 拿不到，但能复用华为云登录态 | B：仅 credits 窗口，无用量 |
| 全失败 | 不集成，归档探测证据 |

---

## 9. 一期不做

token 用量 client（二期）、session detail、tray 专属图标、WSL（IDEA 无 WSL 形态）。

二期若走用量 client：它不是"解析本地文件"而是"调本地 HTTP 再映射成 tokscale 形状 entry"——**没有现成先例**（`promaUsage.js` 读文件、`qoderCnUsage.js` 读 SQLite）。tokens 字段可参照 `opencodeSession.js:117-145` 的 opencode 模型：

```
total = input + output + cacheRead + cacheWrite      // L144
// reasoning 只做展示，不进 total（与 Claude/Codex 同口径）
```

---

## 10. 变更清单

**新增（2）**
- `src/shared/codeartsdoerLimits.js`
- `src/electron/codeartsdoerLocalAuth.js`

**修改（9）**
- `src/shared/limitProviders.js`（L5-10 加 id）
- `src/shared/limitCollector.js`（L3952-3977 注册表）
- `src/electron/main.js`（L51 / L681-690 / L742）
- `src/electron/runtimeConfig.js`（L87 / L149-213）
- `src/electron/renderer/app.js`（L82）
- `src/electron/renderer/limitProviderPresentation.js`（L38 / L70 / L289）
- `docs/API.md`（`limits.providers[].provider` 枚举 + `source: 'local'` 语义）
- `README.md` × 5 语言 + `.env.example`（Supported Tools 表 + 正文计数，`readmeConsistency.test.js` 会查）
- `worker/src/shared/{limitProviders,limits}.js`（`npm run sync:worker` 生成，CI 查漂移）

**测试（4）**：见 §7。
