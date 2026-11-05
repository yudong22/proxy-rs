# proxy-rs 功能规划：客户端访问账户（入站鉴权）+ 账号池导出集成

> 本文不绑定具体版本号（当前主线 2.4.1）。它是上一轮「公网 IP + 域名 Windows 服务器发布」梳理的**安全前置**：那轮结论里「`/v1/*` 数据面没有客户端鉴权」是公网暴露的头号阻塞项，本规划负责补上。
>
> 范围经确认：**仅做入站 key 校验**（不做配额、并发限制、按账户计费统计）；管理入口 **GUI + HTTP 双通道**；**新账户必须与现有账号池导出/导入一起迁移**；本次只产出计划，不改代码。
>
> **核心约束（用户明确要求）**：上游 key 与网关下游（客户端）key **分开管理、互不改动**。`x-api-key` / `authorization` 继续保留给 `UPSTREAM_API_KEY_PASSTHROUGH` 做上游透传，**不改这个行为**；网关的客户端访问 key 走**独立的专用 header**。两者可以同时启用。
>
> **部署通道（用户指定）**：通过 `~/.ssh/config` 里的 `qmt` 别名（`Host qmt qmtnew` → 腾讯云 Windows Server）远程操作。目标机实测现状、测试分层与部署步骤见 §7–§9。**该机无 Rust/Node/Caddy/IIS/WebView2，C 盘仅剩 6GB**，因此必须本地交叉编译 + headless 二进制 + E 盘落地。
>
> **已实测验证（2026-11-05，本机 macOS aarch64）**：Windows 交叉编译链已打通 —— `cargo-xwin` + `CC=clang` + `AR=llvm-lib` + `RUSTFLAGS=-C target-feature=+crt-static` 可产出 **PE32+ 单 exe，仅依赖 KERNEL32/ntdll，无需 VC++ 运行库**；`rusqlite bundled` 静态 SQLite 在 crt-static 下正常。过程中修掉了 `src/util.rs` 的 Windows 时区编译错误（`libc::localtime_r` → `localtime_s`，注意参数顺序与 `errno_t` 返回值）。详见 §9.3。

---

## 1. 术语：先分清方向，否则一定做错

项目里已经有「账号池 / 身份池」，指的是**出站**的上游凭据。本规划加的是**入站**的客户端凭据，方向相反、用途相反、文件也相反。命名必须从第一天就分开，否则 GUI、日志、导出包会全部混成一团。

| 术语 | 方向 | 现有/新增 | 存储文件 | 作用 |
|---|---|---|---|---|
| 身份池（账号 / 密钥） | **出站** → 上游 | 现有 | `workbuddy-credentials.json`、`api-keys.json`、`workbuddy-pool.json` | 网关拿什么身份去请求 WorkBuddy / 上游模型 |
| **访问账户（Client Account）** | **入站** ← 客户端 | **新增** | `client-accounts.json` | 谁可以调用本网关的 `/v1/*`，防止公网被白嫖 |
| `PROXY_API_TOKEN` | 管理面 | 现有 | `.env` / 环境变量 | 保护 `POST /api/scheduler/run/*`，本规划复用它保护管理接口 |

**强制约定**：新增代码、GUI 文案、日志字段、导出包字段一律用 `client_account(s)` / 「访问账户」，禁止复用 `api_keys` / `credentials` / 「密钥池」等现有名字。`api-keys.json` 是**上游密钥池**，不是客户端 key，二者不可合并到一个文件。

**两套 key 的分工（本规划的基石）**：

| | 上游 key（现有，不动） | 网关访问 key（新增） |
|---|---|---|
| 用途 | 客户端透传给上游做鉴权 | 客户端向**本网关**证明身份 |
| 携带位置 | `x-api-key` / `authorization: Bearer` | **专用头 `x-proxy-rs-key`**（备选 `x-gateway-key`） |
| 处理方 | `src/proxy/auth_resolvers.rs` → `auth_headers.rs::upstream_auth_headers` → 出站 | 新增入站中间件，校验后**剥离**，不参与转发 |
| 存储 | `api-keys.json`（身份池）、`GuiSettings.api_key` | `client-accounts.json`（独立） |
| 开启条件 | `UPSTREAM_API_KEY_PASSTHROUGH` / 身份池 | `client_auth_enabled`（独立开关） |

两者可同时启用、互不感知：一次请求可以同时带 `x-api-key: <上游key>` 和 `x-proxy-rs-key: <网关访问key>`，前者原样走上游，后者在网关层被消费掉。这样既不破坏存量透传部署，又让公网网关有了自己的门禁。

---

## 2. 目标与非目标

### 2.1 目标

| # | 目标 | 验收方式 |
|---|---|---|
| G1 | 公网暴露下，未持有效访问 key 的请求拿不到任何上游额度 | `curl https://域名/v1/messages`（无 `x-proxy-rs-key`）→ 401；带正确 key → 200 |
| G2 | 访问账户可增删、启禁用、单独吊销，改动**无需重启网关** | GUI 点「禁用」后立即 401；重新启用后立即 200 |
| G3 | 访问账户与身份池**同一个导出文件**一起跨机迁移，且旧版导出包仍可导入 | v2 包往返后账户全部可用；v1 包导入不报错、不丢账号/密钥 |
| G4 | 加密导出包中访问账户凭据不出现在明文 envelope | 加密包全文 grep 不到 key/hash，只有 counts |
| G5 | 升级不破坏单机自用用户 | 默认关闭入站鉴权；未开启时所有现有 CLI 配置原样可用 |
| G6 | 请求日志能追溯到「哪个账户发的」 | `proxy.log` 与 GUI 实时日志出现 `client_account=<label>` |
| G7 | CLI 一键写入配置能带上真实访问 key | 「写入 Claude 配置」/ Codex 配置产出可用配置，且在**透传模式下仍保留上游 key** |
| G8 | **上游透传零改动** | `UPSTREAM_API_KEY_PASSTHROUGH=true` 时，`x-api-key` / `authorization` 仍原样作为上游密钥；开启客户端鉴权不影响它，二者可同开 |

### 2.2 非目标（本版不做）

- 每账户配额、QPS/并发上限、按账户计费、额度看板 —— 属于「完整多租户」，范围外。
- 账户角色/权限分级、账户自助申请与审批流。
- TLS 终止（由 Caddy / IIS 反代负责，网关仍是纯 HTTP）。
- 修改 `/api/*` 只读观测接口的鉴权模型（理由见 §4.4）。
- **不改动** `x-api-key` / `authorization` 的现有语义、`UPSTREAM_API_KEY_PASSTHROUGH`、`auth_resolvers.rs`、`auth_headers.rs` 的任何行为 —— 上游透传链是既有能力，本规划只在它之外新增一条独立的入站通道。
- 本规划**不实现**上一轮的 Windows 编译修复 / headless 二进制 / CI 改造，但 §10 给出与它们的接线顺序。

---

## 3. 现状摘要（写代码前必读）

| 领域 | 现状 | 对本规划的影响 |
|---|---|---|
| 路由唯一注册点 | `src/router.rs::build_app_router`，`CorsLayer::allow_origin(Any)`（:47） | 中间件加在这里；8 个调用点需同步（见 §3.1） |
| 入站 header 已被占用 | `src/proxy/auth_resolvers.rs` 的 `resolve_api_key` / `resolve_responses_api_key` / `resolve_chat_api_key` 从 `x-api-key` / `authorization` 取值，`UPSTREAM_API_KEY_PASSTHROUGH=true` 时**直接当上游密钥转发** | **不可占用这两个头**，否则会污染透传链。改用独立头 `x-proxy-rs-key`，见 §4.2 / §4.3 |
| 出站头是白名单构造 | `auth_headers.rs::upstream_auth_headers` 只 `insert` 固定的 `authorization` / `x-api-key` / 指纹头，入站 `HeaderMap` **从不整体转发**（`proxy.rs` 仅用 `format_headers` 记日志） | 新专用头天然不会泄漏到上游；仍需在中间件里显式剥离作为纵深防御，见 §4.2 |
| 客户端头形态 | Claude Code 发 `x-api-key`（+ `ANTHROPIC_AUTH_TOKEN`），Codex 发 `Authorization: Bearer` | 这两个头留给上游透传；网关访问 key 走**额外**的 `x-proxy-rs-key`，各客户端需能附加自定义头（§4.3 有兼容矩阵） |
| 已有常量时间比较 | `src/api_token.rs::constant_time_eq`（private） | 提为 `pub(crate)` 复用，禁止再写一份 |
| 已有随机源 | `aes_gcm::aead::{OsRng, RngCore}`，`pool_transfer.rs:40` 已用 | 生成访问 key 复用，不新增 rand 依赖 |
| 已有 hash | 手写 `md5_hex`（仅用于 id 指纹）；`sha2 0.10` 已在 `Cargo.lock`（间接依赖） | 访问 key 用 SHA-256 存 hash；`Cargo.toml` 显式加 `sha2` |
| 敏感头脱敏 | `src/util.rs::format_headers` 已 redact `authorization` / `x-api-key` / 含 `api-key` 等片段（:58-85） | **新头 `x-proxy-rs-key` 必须补进 `SENSITIVE_HEADERS`**，否则访问 key 会明文落 `proxy.log` |
| 导出格式 | `src/pool_transfer.rs`：`BUNDLE_VERSION = 1`（:61），`BundlePayload` / `BundleFile` 三个数据字段，`payload_checksum` 对整个 payload 求 MD5 | 加第四类数据 + 升 v2，见 §5 |
| 导出兼容策略 | `decode()` 对 `version > BUNDLE_VERSION` **只警告不拒绝**（:598） | 旧版读 v2 天然安全，是本方案可行的前提 |
| 导出健康校验 | `workbuddy_auth::store_health()` 返回 3 项（账号池/密钥池/偏好设置），`collect_payload` 遍历它 | 漏加第四项 → 损坏的账户文件导出时被静默吞掉 |
| 导出写入 | `commit()` 写 3 个文件，`write_json_atomic(..., secret=true)` 设 0600 | 第四个文件同样走这条路径 |
| 客户端配置占位符 | `src/claude_config.rs:17` `PLACEHOLDER_AUTH_TOKEN = "nouse"`；`main.rs:1501` 写 `http://127.0.0.1:{port}` | 鉴权开启后需把访问 key 写到自定义头；透传模式下的 `ANTHROPIC_AUTH_TOKEN` 仍是上游 key，两者并存，见 §6.5 |
| GUI 设置页 | `ui/src/views/settings/form.js` 用 `section()` 拼装；`credential-pool.js` 管身份池；`pool-transfer.js` 管导入导出预览 | 新增独立 section 与模块，勿塞进 `credential-pool.js` |
| GUI 门禁 | `ui/tools/check-ui.mjs`（静态 id 唯一性/import/id 引用）、`smoke.mjs` | 新增 DOM id 必须能过 `task ui-test` |
| `Config` 构造 | `src/config.rs:227` 一个全字段 literal + `Default`（:161）；其余 30+ 处用 `..Default::default()` | 加 `client_auth_enabled` 只需改 2 处，其余不受影响 |
| `build_app_router` 调用点 | `src-tauri/src/main.rs:2179` + 7 个集成测试 | 新增参数要一次改全，见 §3.1 |

### 3.1 `build_app_router` 的全部调用点（改动时逐个核对）

```
src-tauri/src/main.rs:2179                 生产
tests/api_endpoints.rs:57
tests/api_scheduler.rs:88
tests/router_redaction.rs:55
tests/body_buffer_limit.rs:53
tests/account_pool_auth.rs:233,306,377,423
tests/upstream_status_relay.rs:99
```

---

## 4. 核心设计

### 4.1 数据模型（`client-accounts.json`）

新增 `src/client_accounts.rs`（Layer 3，I/O 层，与 `workbuddy_auth.rs` 同级）。

```rust
pub struct ClientAccount {
    pub id: String,              // "ca-" + key_hash 前 6 位 hex（可由 hash 重算，见 §5.3）
    pub label: String,           // 用户备注，如 "家里笔记本" / "CI"
    pub key_hash: String,        // sha256(key) 的 hex；不存明文
    pub key_prefix: String,      // key 前 8 位，仅用于 GUI 列表辨认
    pub enabled: bool,           // 默认 true
    pub created_at_ms: i64,
    pub last_used_ms: Option<i64>, // 仅展示「谁在用」，不参与配额
    pub note: String,
}
```

- **key 格式**：`prs_` + 32 字节 `OsRng` 随机 + base64url（约 47 字符）。前缀便于在客户端配置里一眼认出。
- **明文只出现一次**：`create()` 生成后把明文随返回值交给调用方（GUI 弹窗 / HTTP 响应），后端只持久化 `key_hash`。之后任何列表接口都拿不到明文，只能吊销重发。这是标准做法，也是 G4 的前提。
- **id 从 `key_hash` 派生**（`ca-` + 前 6 位），而不是从明文 key 派生：导出包里没有明文，`reconcile_ids` 仍能重算并校验 id，与 `api_key_id(&k.key)` 的角色对等。
- 文件权限：`0600`（Unix）。Windows 上 `set_private_permissions` 是 no-op，依赖用户目录 ACL —— 与现有三个状态文件一致，在文档里如实说明，不假装加密。
- 校验是「遍历 enabled 账户 → `sha256(presented)` → `constant_time_eq`」。账户量级是几十个，线性扫描足够；先用 `key_prefix` 粗筛可省掉大部分 hash 计算。

### 4.2 鉴权中间件与开关

在 `src/router.rs` 用 `axum::middleware::from_fn_with_state`（axum 0.7.9 已确认提供 `from_fn_with_state`，见 `axum-0.7.9/src/middleware/from_fn.rs:158`）加一层，位于 `Extension` 注入之后、`cors` 之前。

- **状态**：`Arc<ClientAuthState>`，持有 `Arc<RwLock<Vec<ClientAccount>>>` + `enabled: bool`。GUI/HTTP 增删改后调用 `reload_from_store()` 热生效，无需重启（G2）。
- **新参数**：`build_app_router(..., client_auth: Arc<ClientAuthState>)`。8 个调用点同步；测试传 `ClientAuthState::disabled()` 保持原行为。
- **开关来源**：`GuiSettings.client_auth_enabled`（`#[serde(default)]`，默认 `false`）+ 环境/` .env` 覆盖 `PROXY_CLIENT_AUTH=1|true`。GUI「服务设置」里放一个显式开关。
  - **默认关闭是硬性要求**：升级后不配任何东西，现有 `ANTHROPIC_BASE_URL=http://localhost:3456 claude` 必须继续能跑，否则一次升级会让所有用户的 CLI 全挂。
  - `client_auth_enabled` 属于**机器/部署决策**，与端口、bind、模型映射同类，`pool_transfer` 的模块注释明确「不导出机器相关配置」——**不要**把它放进导出包。
  - 边界情况：开关开启但账户表为空 → 视为「配置错误」，`/v1/*` 一律 401 并在日志给出「尚未创建任何访问账户」的可操作提示，**绝不 fail-open**。
- **中间件职责**：
  1. 路径分流（见 §4.4）；
  2. **从专用头 `x-proxy-rs-key` 取值**（大小写不敏感）。**不读** `x-api-key` / `authorization` —— 那两个头属于上游透传链，本规划一个字节都不动（§4.3）；
  3. 校验；失败返回 `401` + `WWW-Authenticate: Bearer`；
  4. 成功则 `request.extensions_mut().insert(ClientAccountIdentity { id, label })`，并更新 `last_used_ms`（写回节流，例如≥60s 才落盘，避免每请求写文件）；
  5. **`request.headers_mut().remove("x-proxy-rs-key")` 剥离专用头**：出站头由 `upstream_auth_headers` 白名单构造，本来就不会带上它，这一步是纵深防御 + 防止将来有人改成「透传全部入站头」时把网关访问 key 送给上游；
  6. 放行。
- **401 响应体用 Anthropic 风格**，不是项目通用的 `proxy_error` 信封：
  ```json
  {"type":"error","error":{"type":"authentication_error","message":"Invalid or missing gateway access key (x-proxy-rs-key)"}}
  ```
  理由：Claude Code / Codex 会把 `proxy_error` 当网关故障并重试，`authentication_error` 才会让它干净地停下并提示用户检查配置。这一点和项目已有的「终态错误原样透传」是同一类决策（`docs/release-2.1.0.md`）。**不要**用 502/403。
- **`src/util.rs` 的 `SENSITIVE_HEADERS` 补 `x-proxy-rs-key`**：`format_headers` 用名单 + `api-key` 片段匹配，新头名不含这些片段，不补就会把访问 key 明文写进 `proxy.log`。这是必做项，写进 P3 验收。

### 4.3 上游透传与入站鉴权并存（用户明确要求，不改现有行为）

**决策：`x-api-key` / `authorization` 的语义与 `UPSTREAM_API_KEY_PASSTHROUGH` 完全不动，不做互斥、不做降级、不猜优先级。** 网关访问 key 走独立的 `x-proxy-rs-key`，两套 key 正交、可同时启用。

一次请求的两个通道各走各的：

```
客户端 ──► 网关
  x-api-key: <上游 key>          ─┐
  authorization: Bearer <上游 key> ├─► resolve_*_api_key（原样）─► upstream_auth_headers ─► 上游
  x-proxy-rs-key: <网关访问 key> ─┘   中间件消费 + 剥离（不进入转发链）
```

为什么这样可行（已核实代码）：

- `src/proxy/auth_headers.rs::upstream_auth_headers` 是**白名单构造**：只 `insert` 固定的 `authorization` / `x-api-key` / WorkBuddy 指纹头，入站 `HeaderMap` 从不整体转发（`src/proxy.rs` 只用 `format_headers` 记日志）。专用头天然到不了上游，中间件再显式 remove 一层兜底。
- `auth_resolvers.rs` 的三个 `resolve_*_api_key` 一行都不用改：它们只认自己那两个头，看不到新头。
- 因此 `UPSTREAM_API_KEY_PASSTHROUGH=true`（客户端自带上游 key）+ `client_auth_enabled=true`（网关门禁）可以同时开；静态上游密钥 / 登录态账号池 + 网关门禁同理。**G8 的验收就是这组组合**。

#### 客户端附加自定义头的兼容矩阵（必须逐项验证，这是本方案唯一的现实成本）

| 客户端 | 能否带 `x-proxy-rs-key` | 配置方式 |
|---|---|---|
| `curl` / 自写 HTTP 客户端 | ✅ | `-H "x-proxy-rs-key: $KEY"` |
| Claude Code | ⚠️ 需核实 | `~/.claude/settings.json` 的 `env` 支持 `ANTHROPIC_AUTH_TOKEN` / `ANTHROPIC_BASE_URL`；**自定义请求头支持需实测**。若不支持，见下方兜底 |
| Codex CLI | ⚠️ 需核实 | `~/.codex/config.toml` 的 `[model_providers.*]` 有 `http_headers` 一类键（版本相关，需实测） |
| OpenAI SDK / 通用客户端 | ✅ | 多数支持 `default_headers` / `extra_headers` |
| DSH / cordis patch | ⚠️ 需核实 | `src/dsh_config.rs::upsert_provider` 目前只写 `baseURL` / 模型列表，自定义头能力需确认 |

**兜底方案（若某客户端确实无法加自定义头）**：在**反代层**做头注入 —— Caddy/IIS 对来自可信来源（IP 白名单 / mTLS / 已认证路径）的请求统一补上 `x-proxy-rs-key`。**绝不**因此回头去复用 `x-api-key` / `authorization`，那会破坏用户明确要求保留的透传语义。把兜底写进 §6.4 的 Caddyfile 示例。

> 实现顺序建议：先按「双头并存」做通用路径（curl 可验证），再逐个客户端实测自定义头能力，把结果回填到上表；任何客户端不支持都走反代注入，不改网关代码。

### 4.4 路径边界与公网边界（重要架构判断）

| 路径 | 是否鉴权 | 理由 |
|---|---|---|
| `/v1/messages`、`/v1/responses`、`/v1/chat/completions` 及全部无前缀别名（`/responses`、`/chat/completions`、`/backend-api/codex/responses`） | **✅ 访问 key** | 数据面，直接消耗上游额度 |
| `/v1/models`、`/models`、`/v1/credits`、`/credits` | **✅ 访问 key** | 泄露模型/余额信息，同属数据面 |
| `/health` | ❌ 放行 | 反代与探活需要；内容固定字符串 `OK` |
| `/metrics` | ❌ 放行（**由反代 deny**） | 见下 |
| `/api/stats/today`、`/api/request-logs`、`/api/scheduler` | ❌ 放行（**由反代 deny**） | 桌面 GUI 通过 `fetch` 直接读这些接口（`README.md:311`）；若网关层加鉴权，本机 GUI 会 401。改动面大且收益低，把这道边界放在反代更干净 |
| `/api/scheduler/run/:loop_id` | `PROXY_API_TOKEN`（现有） | 不变 |
| `/api/admin/client-accounts/*` **（新增）** | **`PROXY_API_TOKEN` + 无 Origin** | 管理面，复用 `api_token::authorize`；curl/脚本调用不带 Origin ✅ |

> 结论：**公网边界由反代负责隐藏 `/api/*` 与 `/metrics`，网关层只负责 `/v1/*` 的入站 key 校验**。这与上一轮梳理的「反代只暴露 `/v1/*`」一致，且避免把 GUI 的本机 fetch 卷进鉴权改造。Caddy 片段见 §6.6。

### 4.5 日志可追溯（G6）

- 中间件把 `ClientAccountIdentity` 塞进 extensions；`src/proxy.rs` 三个 handler（:100 / :266 / :424）现有的 `incoming_headers` INFO 行末尾追加 `client_account=<label>`（各一行改动）。中间件在剥离专用头**之后**才到 handler，所以这行日志里看不到 `x-proxy-rs-key`；账户名由 extension 提供。
- `util::format_headers` 已脱敏 `x-api-key` / `authorization`；**必须**把 `x-proxy-rs-key` 补进 `SENSITIVE_HEADERS`，否则开关关闭时（中间件不剥离）或调试分支会把访问 key 明文写进 `proxy.log`。补一条 `tests/router_redaction.rs` 用例钉住。
- **不新增 `stats.db` 列**：`RequestOutcome` 被 7 个集成测试与大量单测显式构造，加字段会波及全仓；本期「日志记账户名」通过 `proxy.log` + GUI 实时日志满足。若后续要做按账户统计，再单独立项走 `stats.rs::migrate` 的现有 TEXT 列扩展模式（:890）。

---

## 5. 导出/导入集成（本次新增的硬要求）

### 5.1 格式版本策略

`src/pool_transfer.rs` 的模块注释写明「三个文件、版本独立于 app 版本」。加第四类数据属于 layout 变化，按约定升版：

- `BUNDLE_VERSION: 1 → 2`。
- `BundlePayload`、`BundleFile` 各加一个 `#[serde(default)] client_accounts: Vec<ClientAccount>`；`BundleFile` 用 `#[serde(default, skip_serializing_if = "Vec::is_empty")]`。
- `BundleCounts` 加 `client_accounts: usize`（`#[serde(default)]`，保证旧版反序列化 v2 不炸）。

**双向兼容由现有机制保证**（这是选升版而不是「悄悄加字段」的原因）：

| 方向 | 行为 |
|---|---|
| 新版读 v1 旧包 | 新字段 `default` → 空 Vec，账号/密钥/偏好照常导入 ✅ |
| 旧版读 v2 新包 | `decode()` 命中 `version > BUNDLE_VERSION` → 警告「可能丢失部分内容」，**不拒绝**，忽略未知字段 ✅ |
| v2 明文包 | `client_accounts` 与其他三块并列，`payload_checksum` 自动覆盖（它序列化整个 `BundlePayload`）✅ |
| v2 加密包 | payload 整体进 AES-256-GCM，`encrypt_payload` / `decrypt_payload` **零改动**；但 `export_text` 必须像清空 `credentials`/`api_keys` 一样清空 `bundle.client_accounts`，counts 留在 envelope ✅ |

### 5.2 逐函数改动清单（`src/pool_transfer.rs`）

| 函数 / 类型 | 改动 |
|---|---|
| 模块头注释（:1-13） | 「三个文件」→「四个文件」，补一段说明访问账户是**入站**凭据、与上游身份池方向相反 |
| `BUNDLE_VERSION`（:61） | `1 → 2` |
| `BundlePayload`（:128） | 加 `client_accounts` |
| `BundleCounts`（:140） | 加 `client_accounts` |
| `BundleFile`（:166） | 加 `client_accounts`（明文分支） |
| `ExportOutcome::is_empty`（:216） | 纳入 `counts.client_accounts`，否则「只配了访问账户」会被误报为空包 |
| `ImportPreview`（:224） | 加 `client_accounts_total/added/updated/unchanged/removed` 五个字段 |
| `PlannedState`（:252） | 加 `client_accounts: Vec<ClientAccount>` |
| `collect_payload()`（:286） | 增加 `client_accounts::load_client_accounts()`；空包警告条件纳入它 |
| `export_text()`（:349） | counts 加项；加密分支清空 envelope 的 `client_accounts` |
| `decode()`（:608） | 明文 `BundlePayload` 构造补 `client_accounts: bundle.client_accounts.clone()` |
| `reconcile_ids()`（:652） | 新增：按 `key_hash` 重算 `ca-` id（**不能**像 `api_key_id(&k.key)` 那样从明文重算）、修复引用、按 id 去重并给出中文警告 |
| `drop_empty()`（:736） | 跳过 `key_hash` 为空的账户 |
| `plan()`（:760） | 第四组 merge/replace 统计；**注意 `default_identity_id` 的 `known()` 判定不要误纳入访问账户**（两套 id 空间互不引用） |
| `commit()`（:1006） | 第四个 `write_json_atomic(..., secret=true)`，写入顺序放在三个身份池文件之后；备份逻辑自动生效 |
| `store_health()`（`src/workbuddy_auth.rs:254`，或新模块自带） | 加第四项「访问账户」，否则损坏文件导出时被静默吞掉 |

配套：`workbuddy_auth::store_health()` 的 `match name` 分支（`pool_transfer.rs:298`）补 `"访问账户" => client_accounts::path()`。

### 5.3 为什么不存明文反而更利于导出

导出的是 `key_hash` + `key_prefix` + `id`，导入到新机器后**客户端手里的 key 不变、仍能校验通过**（校验本来就是比 hash）。所以「只存 hash」与「跨机迁移」不冲突，反而让加密包和明文包都更安全。`id = "ca-" + key_hash[..6]` 保证 `reconcile_ids` 在没有明文的情况下依然能自愈，这与 `api-keys.json` 那套 `k-<6hex>` 语义对称。

### 5.4 需要同步的导出周边

- `ui/src/views/settings/pool-transfer.js`：`renderPreview()` 加账户行；导出计数（:210-219）加「访问账户 N 个」；导出成功文案（:296）加计数。
- `ui/src/views/settings/form.js`：导出/导入区块的说明文案改为「四个文件」。
- `src-tauri/src/main.rs::pool_export / pool_import_preview / pool_import`：本身透传 `Value`，多数无需改；核对 `pool_export` 里手写的 counts 展示（:1973 附近）是否另有一份硬编码。
- `README.md`：数据持久化目录表加 `client-accounts.json` 一行；「跨机迁移」段落（:284）「三个文件」→「四个文件」；环境变量速查表加 `PROXY_CLIENT_AUTH`；HTTP 接口全览加 `/api/admin/client-accounts`。

---

## 6. 分阶段工作计划

> 依赖顺序：P1 是唯一根节点，P2/P3/P4 都只依赖它，可并行；P5 依赖 P3+P4 的 key 生成入口；P6 依赖 P2/P3；P7 贯穿全程。
>
> 每阶段的测试要求见 §8（P1/P2/P3 各自带单测与集成测试，**测试与被测代码同阶段交付**，不留到 P7 补）；P8 是远端部署，须在 §10 的两项 Windows 编译前置修完后进行。

### P1 — 存储层与 crypto 基础（无外部依赖，先做）

| 任务 | 文件 | 验收 |
|---|---|---|
| 1.1 `Cargo.toml` 显式加 `sha2 = "0.10"`（已在 lock，无新增依赖树）；确认 `subtle`/`base64` 可用 | `Cargo.toml` | `cargo tree -i sha2` 正常 |
| 1.2 新建 `ClientAccount` / `load` / `save` / `create` / `upsert` / `delete` / `set_enabled` / `reload_from_store`，路径走 `settings::data_dir()`（自动继承 `PROXY_DATA_DIR` 与 `HOME` 兜底行为） | `src/client_accounts.rs`（新） | 单测：创建→落盘→重载，明文不出现在文件里 |
| 1.3 `id_from_hash` / `verify(presented, store)` / `masked()`，比较复用 `api_token::constant_time_eq` | 同上、`src/api_token.rs`（提 `pub(crate)`） | 单测：正确 key 通过、错一位失败、空 key 失败、禁用账户失败 |
| 1.4 `lib.rs` 注册 `pub mod client_accounts` | `src/lib.rs` | `cargo check` |
| 1.5 `GuiSettings` 加 `#[serde(default)] client_auth_enabled: bool`，`Config` 加同名字段并接 `PROXY_CLIENT_AUTH`。**不与 `passthrough_api_key` 做任何互斥/降级**，两者独立共存（§4.3） | `src/settings.rs`、`src/config.rs` | 单测：默认 false、env 覆盖生效、**与 passthrough 同开时两个字段都为 true 且无报错** |

### P2 — 导出/导入集成（只依赖 P1，可并行推进）

| 任务 | 文件 | 验收 |
|---|---|---|
| 2.1 `BUNDLE_VERSION` 升 2，`BundlePayload`/`BundleFile`/`BundleCounts`/`ImportPreview`/`PlannedState`/`ExportOutcome` 加字段 | `src/pool_transfer.rs` | 编译通过 |
| 2.2 `collect_payload` / `export_text` / `decode` / `reconcile_ids` / `drop_empty` / `plan` / `commit` 全链路接入第四类数据 | 同上 | 见 2.5 |
| 2.3 `store_health` 加「访问账户」+ `pool_transfer.rs:298` 的 match 分支 | `src/workbuddy_auth.rs`、`src/pool_transfer.rs` | 损坏文件导出时产生中文警告而非静默 |
| 2.4 模块头注释、README 表格与「跨机迁移」段落 | `src/pool_transfer.rs`、`README.md` | 文档与代码一致 |
| 2.5 测试 | `src/pool_transfer.rs` 内 `mod tests`、`tests/pool_transfer_roundtrip.rs` | 新增用例：① v2 含账户明文往返；② v2 加密往返且 envelope 不含 hash/key；③ **v1 旧包导入不报错、账号密钥不丢**；④ **模拟旧版读 v2**：`version = BUNDLE_VERSION + 7` 走「警告不拒绝」路径；⑤ merge 只增不删 / replace 清空本机账户；⑥ 损坏账户文件被 `store_health` 捕获；⑦ id 与 hash 不一致时重算；⑧ 重复 id 去重 |

### P3 — 鉴权中间件与路由接线（依赖 P1）

| 任务 | 文件 | 验收 |
|---|---|---|
| 3.1 `ClientAuthState`（`Arc<RwLock<Vec<_>>>` + enabled + `reload_from_store`） | `src/client_accounts.rs` | 增删改后 `reload` 立即可见 |
| 3.2 `guard` 中间件：路径分流、**只读 `x-proxy-rs-key`**（不碰 `x-api-key` / `authorization`）、校验、401 响应体、extension 注入、`last_used_ms` 节流写回、**成功后 `headers_mut().remove` 剥离专用头** | `src/client_accounts.rs`（或 `src/client_auth.rs`） | 单测覆盖 §4.4 路径表每一行；断言请求里同时带 `x-api-key` 时其值不受影响 |
| 3.3 `build_app_router` 增加 `client_auth` 参数并 `.layer(middleware::from_fn_with_state(...))`；CORS 保持在最外层 | `src/router.rs` | 401 响应带 CORS 头 |
| 3.4 **同步 8 个调用点**（§3.1 清单） | `src-tauri/src/main.rs` + 7 个 `tests/*.rs` | 全量测试绿 |
| 3.5 三个 handler 的 INFO 日志追加 `client_account=<label>` | `src/proxy.rs:100/266/424` | 日志出现账户名，且 key 本身被 redact |
| 3.6 新增集成测试：无专用头→401、错误 key→401、禁用账户→401、正确 key→200、`/health` 始终 200、开关关闭时无 key→200；**G8 组合用例**：`passthrough + client_auth` 同开时，上游收到的 `x-api-key` 仍是客户端原值、`x-proxy-rs-key` 不出现在出站头 | `tests/client_auth.rs`（新，仿 `tests/api_scheduler.rs` 的 `Isolated` + 全局锁写法）、`tests/account_pool_auth.rs`（出站头断言） | `task test` 全绿 |
| 3.7 `SENSITIVE_HEADERS` 补 `x-proxy-rs-key`，并加一条 redaction 用例 | `src/util.rs`、`tests/router_redaction.rs` | `proxy.log` 中不出现明文访问 key（含开关关闭时） |

### P4 — GUI 管理入口（依赖 P1）

| 任务 | 文件 | 验收 |
|---|---|---|
| 4.1 `section('client-accounts', '访问账户 (客户端接入)')`，**紧挨身份池但用醒目提示说明方向相反、且明确「不影响上游密钥透传」** | `ui/src/views/settings/form.js` | 面板可见 |
| 4.2 列表 / 添加 / 启禁用 / 删除 / 复制；**新建成功用 modal 一次性展示明文 key + 完整请求头示例 `x-proxy-rs-key: <key>` + 「关闭后不再显示」提示** | `ui/src/views/settings/client-accounts.js`（新） | 手动 + `smoke.mjs` |
| 4.3 「启用入站鉴权」开关（独立开关，与「上游密钥透传」并列且可同开），文案说明两者互不干扰 | `form.js` / `save.js` | 两开关同开可正常保存并生效 |
| 4.4 Tauri 命令 `client_accounts_list/add/toggle/delete` + `reload_from_store` 热生效，注册进 `generate_handler!` | `src-tauri/src/main.rs:2537` | G2：禁用后立即 401 |
| 4.5 `pool-transfer.js` 预览与计数（与 P2 合并做，避免两次改同一文件） | `ui/src/views/settings/pool-transfer.js` | 导出弹窗显示账户数 |
| 4.6 `task ui-check` / `ui-smoke` 同步新增 id | `ui/tools/*` | `task ui-test` 绿 |

### P5 — HTTP 管理入口（依赖 P1；与 P3/P4 可并行）

| 任务 | 文件 | 验收 |
|---|---|---|
| 5.1 `GET /api/admin/client-accounts`（列表，脱敏）、`POST`（创建，**唯一一次返回明文 key**）、`PATCH /api/admin/client-accounts/:id`（启禁用/改备注）、`DELETE` | `src/api.rs`（新增 handler）+ `src/router.rs` 注册 | `curl` 全流程 |
| 5.2 每个 handler 首行调用 `api_token::authorize`，复用 `typed_error_response` 的三档错误（未配置/带 Origin/token 错） | `src/api.rs` | 无 token→403/401；带 `Origin`→403 |
| 5.3 写操作后 `ClientAuthState::reload_from_store()` | `src/api.rs` | 无需重启 |
| 5.4 集成测试：鉴权三档 + CRUD + 创建后新 key 立即可用于 `/v1/*` | `tests/client_auth.rs` | 全绿 |
| 5.5 接口写进 `docs/scheduler-api.md` 同级新文档 `docs/client-accounts-api.md`（请求/响应/错误码/示例 curl） | `docs/`（新） | 文档可照抄 |

### P6 — 客户端配置、反代与文档（依赖 P2/P3/P4）

| 任务 | 文件 | 验收 |
|---|---|---|
| 6.1 客户端配置写入：先把 §4.3 兼容矩阵**逐个实测**（Claude Code / Codex / DSH 能否附自定义头），结果回填文档；支持者写 `x-proxy-rs-key`，**`ANTHROPIC_AUTH_TOKEN` / `PLACEHOLDER_AUTH_TOKEN` 保持原样不动**（透传模式下它就是上游 key，改了会破坏用户明确要求保留的行为） | `src/claude_config.rs`、`src/codex_config.rs`、`src/dsh_config.rs`、`docs/` | 生成的配置可直接跑通；`ANTHROPIC_AUTH_TOKEN` 原值不变 |
| 6.2 Codex `config.toml` 的 provider 补 `http_headers`（若该版本支持）；`base_url` 支持填公网域名（当前 `main.rs:1712` 硬编码 `127.0.0.1`） | `src/codex_config.rs`、`src/dsh_config.rs` 相关写入 | `codex` 从公网域名可用 |
| 6.3 GUI 提供「复制 CLI 配置片段」按钮（含 `ANTHROPIC_BASE_URL` + **`x-proxy-rs-key: <key>`**，透传模式再附 `ANTHROPIC_AUTH_TOKEN=<上游key>`），解决 key 只展示一次之后的取用问题 | `client-accounts.js` | 一键复制可用，两种 key 在片段里分列 |
| 6.4 Caddyfile 示例进 `docs/`：TLS 自动续期、`proxy_buffering off`（SSE 必需）、`read_timeout` ≥ 网关上游 300s、`@deny path /api/* /metrics /api/admin/*` → `respond 404`、只反代 `/v1/*` 及别名；**另附一段可选「可信来源自动注入 `x-proxy-rs-key`」配置**，作为客户端无法加自定义头时的兜底（§4.3），并注明注入条件必须是 IP 白名单/mTLS 而非任意请求 | `docs/`（新） | 照抄可上线；兜底段默认注释掉 |
| 6.5 README 更新：环境变量表（加 `PROXY_CLIENT_AUTH`）、HTTP 接口全览、数据目录表（加 `client-accounts.json`）、跨机迁移段落、「公网部署」小节（明确「反代挡观测接口 + 网关层校 `/v1/*`」双层边界，以及**上游透传与网关门禁是两条独立通道**） | `README.md` | 文档自洽，无「二选一」表述 |
| 6.6 上线验证清单：`/health` 200 → 无专用头 401 → 正确 key 流式 200（首 token 正常）→ 公网访问 `/api/*` 404 → **透传 + 门禁同开时上游 key 原样到达** → 导出/导入到第二台机器后客户端 key 仍可用 | `docs/` | 逐条打勾 |

### P7 — 全量验收

- `task check`（fmt + clippy `-D warnings` + cargo test + ui-test）全绿。
- 回归重点：`tests/account_pool_auth.rs`（鉴权头语义）、`tests/router_redaction.rs`（脱敏）、`tests/api_endpoints.rs`（`/api/*` 不受服务启停门禁）、`tests/body_buffer_limit.rs`（413 不被中间件抢先改成 401）、`tests/upstream_status_relay.rs`（400/413/422 透传不被 401 掩盖）。
- 手工回归：**不开启**入站鉴权时，现有三协议 CLI 配置原样可用（G5）。
- 导出兼容矩阵：v1→v2、v2→v2、v2→旧版、明文/加密 × merge/replace 共 8 组。

### P8 — 远端部署到 qmt（依赖 §10 的两项编译前置 + P1–P5）

真机操作清单，详细步骤与踩坑见 §7 / §8.4 / §9。

| 任务 | 验收 |
|---|---|
| 8.1 `deploy/push-to-qmt.sh`：`ssh -n` + `-EncodedCommand` + scp 后远端 MD5 比对 | 推三个文件全部 MD5 一致；故意改坏一个能报错 |
| 8.2 交叉编译 crt-static 产物 → `E:\proxy-rs\bin\` | 远端直接双击/`Start-Process` 能跑，不报缺 DLL |
| 8.3 写 `E:\proxy-rs\data\.env`（`PROXY_BIND=127.0.0.1`、`PROXY_CLIENT_AUTH=true`、`PROXY_API_TOKEN`） | 前台试跑 `/health` 200、无 key 401、有 key 非 401 |
| 8.4 `Register-ScheduledTask ProxyRS-Gateway`（AtStartup + Highest）+ 守护脚本 | 重启机器后无需登录即监听 3456 |
| 8.5 装 Caddy（单 exe 放 `E:\proxy-rs\bin\`）+ `deploy/win/Caddyfile` | 80/443 通、证书签发、`flush_interval -1` 生效（流式不缓冲） |
| 8.6 防火墙 + 云安全组双处放行（CIDR 白名单） | 公网 `https://域名/health` = OK；`/api/*` = 404 |
| 8.7 跑 `deploy/win/smoke.ps1` 全 9 步（§8.4） | 逐步断言通过，含跨机迁移后原 key 仍可用 |
| 8.8 记录实测结果回填 §7 表格 | 文档与真机一致，不留「注释写 2016、实际 2019」这类过时描述 |

---

## 7. 目标服务器实测现状（部署方案的事实基础）

以下全部由 `ssh qmt` **只读探测**得到（2026-11-05），不是推测。`qmt` 是 `~/.ssh/config` 里的 Host 别名，`Host qmt qmtnew` 指向同一台机器（两个别名等价）。

| 探测项 | 实测结果 | 对部署的硬性影响 |
|---|---|---|
| 主机名 / OS | `10_0_0_8` / **Windows Server 2019 Datacenter** x64（ssh config 注释写的 2016 已过时） | 基线；不要按 2016 的组件版本假设 |
| 登录用户 | `10_0_0_8\administrator` | 数据目录落在 `C:\Users\Administrator\`，权限按 ACL |
| 默认 shell | **PowerShell 5.1**（`HKLM:\SOFTWARE\OpenSSH` 的 `DefaultShell` 指向 powershell.exe） | 远端脚本写 `.ps1` 并用 `-ExecutionPolicy Bypass -File`；**不是** cmd，也不是 pwsh 7 |
| `cargo` / `rustc` | **均无** | **不能在远端编译**。必须本地交叉编译后推产物 |
| `node` | **无** | 远端跑不了 `ui/tools/*.mjs`；前端检查只能在本地或 CI 做 |
| `caddy` / `nginx` / IIS | **全部没有**；80/443 均未监听 | 反代要从零装（推荐 Caddy 单 exe）；或干脆先用 IP:端口直连 |
| 监听端口 | 仅 **22**（OpenSSH）与 **3389**（RDP） | 3456 空闲可用；80/443 需自行开放 |
| WebView2 Runtime | **未安装** | 桌面 GUI 版跑不起来；**headless 二进制没有这个依赖**，这是选 headless 的决定性理由 |
| VC++ x64 运行库 | **未找到** | MSVC 目标产物会缺 `vcruntime140.dll`；见 §9.3 的两种解法 |
| 已有计划任务 | 有一个名为 `Proxy` 的任务（State=Ready），与 proxy-rs **无关**（不是本项目部署物） | 命名要避开 `Proxy`，用 `ProxyRS-*` 前缀，避免误操作别人的任务 |
| 磁盘 | C: 35GB 仅剩 **6GB**；E: 34GB 剩 22GB；D: 空光驱（0 字节） | **产物与数据放 E:**，C 盘只放不下 Rust 构建缓存；D 盘不可写 |
| 目录 | `C:\proxy-rs`、`D:\proxy-rs` 均不存在 | 全新部署，无历史包袱 |

**结论（三条被实测推翻的假设）**：

1. ~~远端直接 `cargo build`~~ → 远端无 Rust 工具链，且 C 盘 6GB 装不下 cargo 缓存。**必须本地交叉编译**。
2. ~~装 Tauri 桌面版~~ → 无 WebView2，且 Server Core 类环境没有交互桌面。**必须 headless 二进制**。
3. ~~用 IIS 做反代~~ → IIS 未安装。**Caddy 单文件**是最低摩擦选择（也便于配 SSE 的 `flush_interval -1`）。

---

## 8. 测试计划（分层，全部可在 `ssh qmt` 前完成）

> 原则：能在本地跑完的绝不推远端；远端只做「真跑起来才暴露」的那一层。这样每轮迭代不必等 SSH。

### 8.1 单元测试（`src/client_accounts.rs` 内 `mod tests`）

| 用例 | 断言 |
|---|---|
| `create` 后落盘 | 文件内容**不含**明文 key；含 `key_hash` / `key_prefix` |
| `verify` 正确 key | 返回该账户 identity |
| `verify` 错一位 / 空 / 大小写变体 | 全部失败 |
| `verify` 对 disabled 账户 | 失败（即使 key 正确） |
| 双账户同前缀 | 仍能正确区分（前缀只做粗筛，不参与判定） |
| `id_from_hash` 幂等 | 同一 hash 恒等；改动一字节即变 |
| `last_used_ms` 节流 | 60s 内多次校验只落盘一次 |
| 文件权限 | Unix 下 0600（`#[cfg(unix)]`） |
| 损坏 JSON | `load` 不 panic，返回空 + 由 `store_health` 报 Corrupt |

### 8.2 集成测试（`tests/client_auth.rs`，仿 `tests/api_scheduler.rs` 的 `Isolated` + 全局锁）

| 用例 | 断言 |
|---|---|
| 开关关闭 + 无专用头 | `/v1/messages` → 200（G5 回归） |
| 开关开启 + 无专用头 | 401 + `WWW-Authenticate: Bearer` + `authentication_error` 体 |
| 开关开启 + 正确头 | 200，且该请求的 `incoming_headers` 日志含 `client_account=<label>` |
| 开关开启 + 错误/禁用 key | 401 |
| `/health` | 任何开关状态均 200 |
| `/v1/models`、`/v1/credits` | 同样受门禁（数据面） |
| **G8 组合**：`passthrough + client_auth` 同开 | 上游收到的 `x-api-key` 仍是客户端原值；**出站头里没有 `x-proxy-rs-key`** |
| 专用头被剥离 | 记录上游请求头，断言不含 `x-proxy-rs-key` |
| 开但账户表为空 | 401，不 fail-open |
| 413 优先级 | 超大 body 在鉴权失败时返回 401（先鉴权）还是 413（先限流）——**明确固定一种**并钉住，避免中间件与 `DefaultBodyLimit` 顺序漂移 |
| CORS | 401 响应带 `Access-Control-Allow-Origin`，浏览器侧能读到错误 |

> 出站头断言建议直接复用 `tests/account_pool_auth.rs` 已有的起真上游 + 捕获请求的写法，那里已经验证过如何观察 `apply_upstream_auth` 的结果。

### 8.3 导出兼容测试（`tests/pool_transfer_roundtrip.rs` 补充）

8 组矩阵：{v1 旧包, v2 新包} × {明文, 加密} × {merge, replace}。重点断言：

- v1 包（无 `client_accounts` 字段）导入后账户表为**空**且不报错；
- 模拟旧版读 v2：手改 `version = 99`，走「警告不拒绝」分支；
- v2 加密包文件全文**不出现** `key_hash` / `prf_` 前缀；
- 导入本机导出的包后，**原客户端 key 仍能通过 `verify`**（跨机迁移的核心保证）。

### 8.4 远端冒烟（`ssh qmt`，真机）

这是唯一无法本地替代的一层。设计成一个可重复执行的 `.ps1`，逐步断言而不是一堆散命令：

| 步骤 | 命令形态 | 断言 |
|---|---|---|
| 1 推送 | `scp` exe + 配置到 `E:\proxy-rs\` | **`push_verified` 式 MD5 比对**（本仓库无此函数，需新写，见 §9.2） |
| 2 启动 | `Start-Process` 加 `PROXY_DATA_DIR` / `PROXY_BIND=127.0.0.1` | 进程存活；3456 进入 Listen |
| 3 探活 | `Invoke-WebRequest http://127.0.0.1:3456/health` | 200 + `OK` |
| 4 无 key | 同上 POST `/v1/messages` | 401 |
| 5 有 key | 带 `x-proxy-rs-key` | 200 或上游业务错误（**不是 401**） |
| 6 流式 | 带 `stream:true`，测首 token 到达时间 | 不缓冲、不超时 |
| 7 观测接口 | `GET /api/stats/today` | 200（本机回环，反代层才 deny） |
| 8 重启存活 | 杀进程 → 计划任务/服务拉起 | 自动恢复且端口重新监听 |
| 9 跨机迁移 | 本机导出 → scp 到 `E:\` → 远端导入 | 客户端原 key 仍 200 |

> 远端冒烟必须用 `-EncodedCommand` 传 PowerShell 源码（见 §9.2 的踩坑），否则中文路径和引号一定会碎。

---

## 9. 部署方案（本地编译 → 推送 → Windows 常驻）

### 9.1 形态选择：headless 二进制，不是 Tauri 桌面版

实测依据：远端**无 WebView2**、无交互桌面、C 盘仅 6GB。桌面版即使装上 WebView2 也无法在无会话环境稳定运行托盘 + 窗口。

因此 P0 前置（**独立于本规划的入站鉴权**，属于上一轮 Windows 计划的落地）：

- 新增 `src/bin/proxy-rs-server.rs`（或 `src-tauri` 之外的独立 bin），只做：读配置 → `build_app_router` → `axum::serve` → 起三个调度循环。`router.rs` 的注释本来就写了「kept free of GUI types so a headless front-end can reuse it unchanged」，这条路是预留过的。
- 该 bin **不依赖 WebView2 / Tauri**，产物是单个 exe。
- 顺带绕开 `libc::localtime_r` 的 Windows 编译错误（`src/util.rs:366` 的 `#[cfg(not(unix))]` 分支用了 Windows 上不存在的 `localtime_r`/`gmtime_r`）——**但 headless bin 仍链接同一个 `proxy_rs` crate，这个编译错误必须先修**，见 §10。

### 9.2 从 Mac 到 Windows 的推送通道（复用已有运维经验）

`qmt` 别名 + 密钥登录已可用（实测连通）。推送脚本**不要自己发明**，直接继承隔壁 `bigqmt/tools/bigctl` 里踩过坑的六条纪律：

| 坑 | 表现 | 本仓库必须照做 |
|---|---|---|
| ssh 吃 stdin | `while read` 循环只跑第一轮，**静默** | 所有 ssh 带 `-n` |
| 引号四层嵌套 | `bash → ssh → cmd → PowerShell` 后编码碎掉，尤其中文 | 远端 PowerShell 一律走 **`-EncodedCommand`**（UTF-16LE + base64），源码里中文可放心写 |
| 输出带 BOM | `case "$x" in EXIT=*)` 匹配不上，变量静默为空 | 解析前 `LC_ALL=C sed 's/^\xEF\xBB\xBF//'` |
| CLIXML 混入 stderr | 非 UTF-8 字节，macOS BSD sed 在 UTF-8 locale 下**直接报错丢全部输入** | 同上，必须 `LC_ALL=C` |
| scp 静默陈旧 | 目标已存在旧文件时，失败仍被「文件存在」判为成功 | `scp` 退出码 **+ 远端 MD5 比对**，两者都过才算成功 |
| 路径不存在 | scp 到不存在的目录返回非 0（**这条已复测订正**：scp 退出码可信） | 先 `New-Item -ItemType Directory -Force`；Windows 接受正斜杠 `E:/proxy-rs` |

产物路径规划（按实测磁盘）：

```
E:\proxy-rs\bin\proxy-rs-server.exe     # 二进制
E:\proxy-rs\data\                        # PROXY_DATA_DIR（含 client-accounts.json）
E:\proxy-rs\data\.env                    # PROXY_BIND / PROXY_API_TOKEN / PROXY_CLIENT_AUTH
E:\proxy-rs\logs\                        # 若用 Caddy，其日志也放这里
E:\proxy-rs\deploy\                      # 起的 .ps1 / 计划任务 XML
```

**不要放 C 盘**（剩 6GB），也不要碰 D:（空光驱）。

### 9.3 交叉编译链（**已实测打通，2026-11-05**）

远端没有 VC++ 运行库，MSVC 产物会缺 `vcruntime140.dll`。已在 macOS 上**实测验证**下述方案可行，不是推测：

**结论：`cargo-xwin` + `+crt-static` 可产出零依赖单 exe。**

```bash
# 一次性准备（已在本机执行成功）
cargo install cargo-xwin                  # 自动拉取微软 CRT/SDK 头与库
rustup component add llvm-tools           # 提供 llvm-ar
# cc-rs 找的是 llvm-lib 这个名字，把它软链到 llvm-ar（llvm-lib 本就是 llvm-ar 的 MSVC 驱动）
ln -sf "$(rustc --print sysroot)/lib/rustlib/aarch64-apple-darwin/bin/llvm-ar" \
       ~/.local/winbin/llvm-lib

# 构建（每次）
export PATH="$HOME/.cargo/bin:$HOME/.local/winbin:$PATH"
export CC_x86_64_pc_windows_msvc=clang
export AR_x86_64_pc_windows_msvc=llvm-lib
export RUSTFLAGS="-C target-feature=+crt-static"
cargo xwin build --release --target x86_64-pc-windows-msvc
```

**实测记录**：

| 验证项 | 结果 |
|---|---|
| `cargo xwin check --lib`（改前） | ❌ 卡在 `ring` 的 C 编译：`assert.h not found`（无 Windows SDK）→ 装 cargo-xwin 后解决 |
| `cargo xwin check --lib`（改后） | ✅ 通过；`ring` / `rusqlite` / `axum` / `reqwest` 全部编译成功 |
| `+crt-static` **release** 构建 lib | ✅ `rusqlite bundled` 的静态 SQLite 在 crt-static 下正常 |
| 真实**链接出 exe**（含 SQLite + tz 调用） | ✅ `PE32+ executable (console) x86-64, for MS Windows`，2.2 MB |
| exe 的 DLL 依赖 | ✅ 仅 `KERNEL32.dll` / `ntdll.dll` / `api-ms-win-core-synch-l1-2-0.dll`，**无 `vcruntime140.dll`** |
| macOS 侧回归 | ✅ `cargo test --lib util::` 18 passed |

> 链接时会出现 `LNK4099: Cannot use debug info for 'libcmt.lib(...)'` 警告——这是微软预编译库未附带 PDB 所致，**无害**，不影响产物。
>
> 因此 §9.3 原「二选一」的方案 1（静态 CRT）选定，**不需要**在远端装 `vc_redist`。

> 已验证同时修复：`src/util.rs` 的 Windows 时区编译错误（`libc::localtime_r`/`gmtime_r` 在 Windows 不存在）。改法不是简单改名——`localtime_s`/`gmtime_s` 的参数顺序相反（出参在**前**）且返回 `errno_t`（0 为成功）而非指针，已按新签名改写。

### 9.4 常驻与自愈

远端 PowerShell 5.1 环境，两种托管方式：

| 方式 | 命令 | 取舍 |
|---|---|---|
| **计划任务**（推荐） | `Register-ScheduledTask` + `-Trigger (New-ScheduledTaskTrigger -AtStartup)` + `RestartCount`/`RestartInterval` | 无需额外组件；实测机器上已有先例（那个 `Proxy` 任务）；命名用 `ProxyRS-Gateway` 避开冲突 |
| NSSM / WinSW | 装第三方 exe | 更接近「真服务」，但多一个要维护的二进制，且需联网下载 |

计划任务要点：

- **`-AtStartup` + `-RunLevel Highest`**，而不是「用户登录时」——服务器重启后不应等人登录。
- 触发器之外的**自愈**：PowerShell 计划任务原生不支持崩溃重启，用 `while ($true) { Start-Process -Wait ... }` 包一层守护脚本，或用一个每 1 分钟检查端口/进程的补充任务。
- 退出码语义：正常停止 `exit 0`，崩溃非 0；守护脚本据此决定是否重启并写事件日志。

### 9.5 防火墙 / 端口

| 端口 | 用途 | 处理 |
|---|---|---|
| 22 | OpenSSH（已监听） | 保持；**限制来源 IP**（成对白名单用 CIDR，不要单 IP） |
| 3389 | RDP（已监听） | 保持或按需关闭；同样限源 |
| 3456 | 网关 | **只绑 `127.0.0.1`**，由反代接入；不直接对公网开放 |
| 80 | ACME HTTP-01 + 跳转 | 需开放（Caddy 申请证书用） |
| 443 | 对外服务 | 需开放 |

云厂商安全组是**第二道**，Windows 防火墙是**第一道**，两处都要改——只改一处会出现「本机能连、外网超时」。

> 安全组超时（不是拒绝）= 包被丢弃 = 安全组没放行，不是服务挂了。这条排查顺序在 `bigctl doctor` 里已写明，直接沿用。

### 9.6 反代（Caddy，因 IIS 未安装）

Caddy 单 exe 放 `E:\proxy-rs\bin\caddy.exe`，`Caddyfile` 要点：

```caddyfile
你的域名 {
    # SSE 必须：不缓冲，否则流式被攒成一整块
    reverse_proxy /v1/* 127.0.0.1:3456 {
        flush_interval -1
    }
    # 无前缀别名，客户端可能直接用
    reverse_proxy /responses*          127.0.0.1:3456
    reverse_proxy /chat/completions*   127.0.0.1:3456
    reverse_proxy /backend-api/*       127.0.0.1:3456
    # 观测与管理接口一律不对外
    @internal path /api/* /metrics /health
    respond @internal 404
}
```

- `flush_interval -1` 是 SSE 的关键（等价 nginx `proxy_buffering off`）。
- 网关侧上游读超时是 300s，反代超时不得短于它。
- 反代再加一层**来源白名单或 mTLS**，作为「客户端无法附自定义头」时的兜底注入点（§4.3）。
- 证书自动续期由 Caddy 负责，80 端口必须可达。

### 9.7 部署验证清单（照抄可执行）

```
[ ] 本地 cargo build --release --target x86_64-pc-windows-msvc（crt-static）
[ ] 产物 MD5 记录
[ ] ssh qmt 'New-Item -ItemType Directory -Force E:/proxy-rs/{bin,data,deploy}'
[ ] scp 产物到 E:/proxy-rs/bin/  → 远端 MD5 比对一致
[ ] 写 E:/proxy-rs/data/.env（PROXY_BIND=127.0.0.1, PROXY_CLIENT_AUTH=true, PROXY_API_TOKEN=<随机>）
[ ] 前台试跑一次，确认 /health 与 401/200
[ ] Register-ScheduledTask ProxyRS-Gateway（AtStartup, Highest）
[ ] 重启机器 → 无需登录即自动监听 3456
[ ] 装 Caddy → 80/443 → 证书签发成功
[ ] 公网 https://域名/health = OK
[ ] 公网无 key = 401；有 key = 200；流式首 token 正常
[ ] 公网 /api/* = 404（被反代挡）
[ ] 本机导出包 → 远端导入 → 客户端原 key 仍可用
```

---

## 10. 与本规划其余部分的接线顺序

上一轮梳理出的三项代码阻塞（Windows 时区 `libc::localtime_r` 编译错误、`HOME` 依赖、macOS LaunchAgent）与本规划的入站鉴权**互不重叠**，但**必须先修前两项**才能编出 Windows 产物：

1. **修 `src/util.rs` 的 Windows 时区编译错误**（改用 `localtime_s`/`gmtime_s` 或 `chrono`）— 阻塞所有 Windows 产物；
2. **修 `HOME` 依赖**（`data_dir()` 在 `HOME` 缺失时返回 `None`，Windows 默认没有 `HOME`）— 否则 `client-accounts.json` 会掉进当前工作目录。最小规避是部署脚本里设 `PROXY_DATA_DIR`，彻底修法是用 `USERPROFILE` 兜底；
3. 落地本规划 P1–P5（纯 Rust + GUI 逻辑，与平台无关）；
4. 抽 headless server 二进制（§9.1），`build_app_router` 的新参数一并带上，部署脚本里显式设 `PROXY_CLIENT_AUTH=true`（服务器**默认开启**，与桌面默认关闭相反）；
5. 按 §9 推送、托管、配反代；
6. 跑 §8.4 的远端冒烟清单。

> 提醒：`client-accounts.json` 与现有三个状态文件一样，在 Windows 上依赖用户目录 ACL，而 `set_private_permissions` 在非 Unix 是 no-op。以 administrator 运行时数据目录在 `C:\Users\Administrator\`，其他标准用户默认读不到；若改用服务账户（LocalSystem），用 `PROXY_DATA_DIR` 指到 `E:\proxy-rs\data` 并显式收紧 ACL。

> 提醒：`client-accounts.json` 与现有三个状态文件一样，在 Windows 上依赖用户目录 ACL，而 `set_private_permissions` 在非 Unix 是 no-op。若以 Windows 服务（如 LocalSystem）运行，数据目录不要放在会被其他用户读到的位置，用 `PROXY_DATA_DIR` 指到受限目录。

---

## 11. 风险与决策记录

| 风险 / 坑 | 处置 |
|---|---|
| 入站 key 误占用 `x-api-key` / `authorization`，污染上游透传 | **两套 key 走不同头**：上游透传链一行不改，网关 key 走 `x-proxy-rs-key`；不做互斥、不猜优先级（§4.3，用户明确要求） |
| 网关访问 key 泄漏到上游 | 出站头是白名单构造，本就不会带上；中间件仍显式 `remove` 专用头兜底（§4.2 第 5 步） |
| 部分客户端无法附加自定义请求头 | §4.3 兼容矩阵逐个实测；不支持者走反代可信来源注入，**绝不回退去复用 `x-api-key`**（§4.3 / P6.4） |
| `x-proxy-rs-key` 明文写进 `proxy.log` | 新头名不含现有脱敏片段，`SENSITIVE_HEADERS` 必须补它 + redaction 用例（P3.7 / §4.5） |
| 改动 `ANTHROPIC_AUTH_TOKEN` 占位符会破坏透传模式 | 保持 `PLACEHOLDER_AUTH_TOKEN` 与写入逻辑原样；访问 key 只走专用头（P6.1） |
| 默认开启会让所有存量用户升级后 CLI 全挂 | 默认 `false`，headless 部署脚本显式开启（§4.2） |
| 给 `/api/*` 加鉴权会打断桌面 GUI 的本机 `fetch` | 网关层不动观测接口，改由反代 deny（§4.4）——**本规划最容易被误改的一处** |
| 401 被客户端当成网关故障反复重试 | 用 Anthropic `authentication_error` 响应体，不用 502/`proxy_error`（§4.2） |
| 只存 hash 后「导出到新机器客户端 key 失效」 | 校验本来就比 hash；`id` 从 `key_hash` 派生以便 `reconcile_ids` 自愈（§5.3） |
| 加密导出包把账户凭据漏进明文 envelope | `export_text` 加密分支必须清空 `bundle.client_accounts`，与现有三字段同样处理（§5.1） |
| `BUNDLE_VERSION` 升 2 后旧版拒绝导入 | 已确认 `decode()` 对 `version > BUNDLE_VERSION` 只警告不拒绝；补测试钉住（§2.1 G3） |
| `store_health` 漏加第四项 → 损坏文件静默丢失 | 列入 P2.3 验收（§5.2） |
| `build_app_router` 改签名漏改测试调用点 | §3.1 列全 8 处，P3.4 逐项核对 |
| `RequestOutcome` 加字段波及全仓测试 | 本期不碰 `stats.db`，只写日志（§4.5） |
| 明文 key 只在创建时出现一次，用户之后找不到 | GUI 一次性弹窗 + 「复制 CLI 配置片段」+ 支持吊销重发（P4.2 / P6.3） |
| 每请求更新 `last_used_ms` 造成写放大 | ≥60s 节流写回（§4.2） |
| Windows 服务账户下文件权限 | 文档如实说明依赖 ACL + `PROXY_DATA_DIR` 指到受限目录（§10） |
| **远端无 Rust/Node/Caddy/IIS** | 实测确认 → 只能本地交叉编译后推产物，反代装 Caddy 单 exe（§7 / §9.1 / §9.6） |
| **远端无 VC++ 运行库** | 已实测：`cargo-xwin` + `+crt-static` 产出零依赖 exe（仅依赖 KERNEL32/ntdll），无需装 vc_redist（§9.3） |
| **C 盘仅剩 6GB，D 盘是空光驱** | 产物/数据/日志全放 `E:\proxy-rs\`，C 盘只承载用户目录（§9.2） |
| 远端已有名为 `Proxy` 的计划任务 | 命名用 `ProxyRS-*` 前缀，注册/删除前先 `Get-ScheduledTask` 核对，勿误删别人的任务（§7 / §9.4） |
| 计划任务无法崩溃自愈 | 用守护脚本 `while($true){Start-Process -Wait}` 包一层，或补一个每分钟探活任务（§9.4） |
| SSH 引用/编码踩坑（引号碎、BOM、CLIXML、吃 stdin） | 全部沿用 `bigqmt/tools/bigctl` 的六条纪律：`ssh -n`、`-EncodedCommand`、`LC_ALL=C` 去 BOM、MD5 双校验（§9.2） |
| 安全组与 Windows 防火墙只改一处 | 「本机能连、外网超时」；两处都要放行，且白名单用 CIDR（§9.5） |
| ~~headless bin 仍需先修 Windows 编译错误~~ | **已修并验证**：`src/util.rs` 改用 `localtime_s`/`gmtime_s`（注意参数顺序与返回值为 `errno_t`），windows-msvc 目标 `cargo xwin check` 通过，macOS 侧 18 项 util 测试无回归（§9.3） |

---

## 12. 交付物清单

| 类型 | 文件 |
|---|---|
| 新增 Rust 模块 | `src/client_accounts.rs`（或拆 `src/client_auth.rs`） |
| 新增 headless 入口 | `src/bin/proxy-rs-server.rs`（§9.1，独立于本规划的鉴权，属 Windows 落地前置） |
| 新增部署脚本 | `deploy/win/deploy.ps1`（推送+校验+注册任务）、`deploy/win/Caddyfile`、`deploy/win/smoke.ps1`（§8.4 冒烟）、`deploy/push-to-qmt.sh`（本地侧，`ssh -n` + `-EncodedCommand` + MD5 双校验） |
| 新增集成测试 | `tests/client_auth.rs` |
| 新增前端模块 | `ui/src/views/settings/client-accounts.js` |
| 新增文档 | `docs/client-accounts-api.md`、Caddy/Windows 部署示例文档 |
| 修改（核心） | `Cargo.toml`、`src/lib.rs`、`src/config.rs`、`src/settings.rs`、`src/router.rs`、`src/api.rs`、`src/api_token.rs`、`src/proxy.rs`、`src/pool_transfer.rs`、`src/workbuddy_auth.rs`、`src/util.rs` |
| **明确不改** | `src/proxy/auth_resolvers.rs`、`src/proxy/auth_headers.rs`（上游透传链）、`src/claude_config.rs` 的 `PLACEHOLDER_AUTH_TOKEN` 与 `ANTHROPIC_AUTH_TOKEN` 写入语义；`src/codex_config.rs` / `src/dsh_config.rs` 仅在兼容矩阵实测支持自定义头时才动 |
| 修改（GUI / IPC） | `src-tauri/src/main.rs`、`ui/src/views/settings/{form,save,pool-transfer}.js`、`ui/tools/*` |
| 修改（文档） | `README.md` |
| 测试改动 | `tests/pool_transfer_roundtrip.rs` + 7 个现有集成测试的 `build_app_router` 调用点 |
