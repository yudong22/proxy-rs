# 3.3.0 审计 · 模块依赖环与分层可行性

> 目标：为「提高内聚、降低耦合」给出**可执行**的分层方案与拆分步骤。
> 范围：只谈结构，不谈功能漂移（那部分在 `surfaces.md`，本文件不涉及）。
>
> **基线声明（重要）。** 本文件所有数字取自 git `HEAD`（`76230dd`）的**提交快照**，
> 通过 `git archive HEAD | tar -x -C /tmp/baseline` 解出后测量，**不是工作树**。
>
> 原因：审计期间另一个 agent 正在并行改动工作树。截至本文件完成时，
> `git status` 显示 20 项变更，其中与本审计直接相关的有：
> `src/stats.rs`（+78 行，新增 `SessionUsageTotal`）、`src/lib.rs`（**模块数 34 → 35**，
> 新增 `pub mod claude_sessions;`）、`src/admin.rs` / `src/admin_routes.rs` / `src/admin_rpc.rs` /
> `src/console.rs` / `src-tauri/src/main.rs` / `tests/admin_api.rs`，以及新增文件
> `src/claude_sessions.rs`。
>
> 因此**本文件对「34 个模块」的统计是 HEAD 事实**；若在改动后的工作树上复核，
> 会看到 35 个模块、`stats.rs` 略长。§6 的命令按 HEAD 快照复现，数字应完全一致。
> 环的结论不受影响——新增的 `claude_sessions` 只依赖下层模块，未引入新环。
> **已在改动后的工作树上（35 模块）独立复核：仍然是同样那 7 个基本环，边集完全一致。**
> 复核命令见 §6。
>
> 计数口径：`wc -l`；「生产」= 去掉所有 `#[cfg(test)]` 项的代码行（非空行）；
> 「测试」= 落在 `#[cfg(test)]` 项内的行，外加 `src/proxy/tests.rs` 整个文件
> （它通过 `src/proxy.rs:23-24` 的 `#[cfg(test)] mod tests;` 引入，本身不含
> `#[cfg(test)]` 标记，**若按「文件内有无 cfg(test)」统计会被整块误算成生产代码**）。

---

## 0. 先纠正任务描述里三个会误导决策的测量假象

这三条不影响方法论，但会让人对「债务有多大 / 哪里该动手」判断错，必须先说清。

### 0.1 `src/proxy.rs` 不是「只有 22 行生产代码」

用「文件里第一个 `#[cfg(test)]` 之前算生产」这条粗糙规则去扫，`proxy.rs` 会得到
22 行——因为它的第一个 `#[cfg(test)]` 出现在第 23 行，紧跟的是
`mod tests;` 这个**声明**（不是测试体本身）：

```
src/proxy.rs:23  #[cfg(test)]
src/proxy.rs:24  mod tests;
src/proxy.rs:25  (空行)
src/proxy.rs:26  以下是全部生产代码……
```

测试体在**另一个文件** `src/proxy/tests.rs`（2617 行）里，所以「22 行之后全是生产代码」
才是事实。

| 事实 | 数值 |
|---|---:|
| `src/proxy.rs` `wc -l` | 2414 |
| 其中生产代码（去 `mod tests;` 声明与测试属性） | **2250** |
| 其中落在 `proxy.rs` 文件内的测试行 | 11（仅 `json_request` 等 `cfg(test)` 辅助） |
| `src/proxy/` 目录生产代码合计（不含 `tests.rs`，`proxy.rs` + 5 个子文件） | **3454** |
| 其中最大的单体 `forward_request` | 851 行（`src/proxy.rs:1062-1912`） |

**结论：`proxy` 模块的生产代码是 3454 行、6 个文件，不是 22 行。**
`src/proxy.rs` 仍是全库**最大的单个生产文件**（2250 行），`forward_request` 仍是
全库**最大的单个函数**（851 行）。这正是 3.3.0 最该拆的地方，而不是「已经拆好了」。

`src/proxy.rs:1-15` 的模块文档也在讲这件事：「已拆出去的在下面 re-export，
所以 `crate::proxy::X` 全部路径不变」。所以 `proxy` 是**拆了一半**：auth_headers /
auth_resolvers / failover / sse / upstream_errors 已独立，**`proxy.rs` 主体（handler +
`forward_request` + 非流式聚合 + `list_models`）还没拆**。

### 0.2 `settings.rs` / `util.rs` / `workbuddy_auth.rs` 的「生产行」

同一个假象也压低了另外两个文件的数字。正确口径：

| 文件 | `wc -l` | 生产 | 测试 | 任务描述里的数 |
|---|---:|---:|---:|---|
| `src/settings.rs` | 1043 | **557** | 396 | 637 |
| `src/util.rs` | 725 | **428** | 229 | 463 |
| `src/workbuddy_auth.rs` | 3842 | **2515** | 1040 | 2705 |
| `src/stats.rs` | 3351 | 1528 | 1570 | 1639 |
| `src/pool_transfer.rs` | 2265 | 1167 | 915 | 1255 |
| `src/credits.rs` | 822 | 700 | 72 | — |

（任务描述的数 = 「第一个 `#[cfg(test)]` 的行号 - 1」，把生产代码的尾部空白与
`#[cfg(test)]` 属性行前面的几行也算进去了，所以普遍偏大几十行。**排序结论不变**，
而且 `workbuddy_auth` 依然是第一。）

全库合计（34 个模块，`src/**/*.rs`）：

| | 行数 |
|---|---:|
| `wc -l` | 42625 |
| 生产 | **25563** |
| 测试 | **13828** |

测试占总行数 **32.4%**，是生产代码的 **54.1%**；共 704 个 `#[test]`/`#[tokio::test]`
（`grep -rho` 实测）。这是本任务第 4 问「哪些拆分会让测试大幅改动」的核心约束——
**有 3 个文件的测试行数已经超过生产行数**（实测）：`translate/pipeline.rs`
（1224 测试 vs 687 生产，多 537）、`translate/responses.rs`（1160 vs 981，多 179）、
`stats.rs`（1570 vs 1528，多 42）——前两个都在**纯函数翻译核心**里，恰恰是拆分时
最依赖「测试能否原地不动」的地方。

### 0.3 环有 **7 个基本环**，不是 5 个；而且按 SCC 看是 3 个强连通分量

任务描述列了 5 条。用 Johnson 式基本环枚举（排除注释/字符串/测试代码后）实测是 **7 条**，
**且多出的第 6、7 条正是最容易漏、代价最低的那两条**：

| # | 基本环 | 长度 |
|---|---|---:|
| 1 | `admin → admin_routes → admin` | 2 |
| 2 | `client_accounts → config → client_accounts` | 2 |
| 3 | `api_token → config → client_accounts → api_token` | 3 |
| 4 | `config → workbuddy_auth → credits → config` | 3 |
| 5 | `credits → session_pool → workbuddy_auth → credits` | 3 |
| 6 | **`client_accounts → workbuddy_auth → credits → config → client_accounts`** | 4 |
| 7 | **`models → stats → settings → providers → translate → models`** | 5 |

按强连通分量（SCC）合并，全库只有 3 个环团：

```
SCC-A (6 个模块)：api_token, client_accounts, config, credits, session_pool, workbuddy_auth
SCC-B (5 个模块)：models, providers, settings, stats, translate
SCC-C (2 个模块)：admin, admin_routes
```

**SCC-B 完全不在任务描述的清单里**，但它是唯一横跨「纯翻译核心」与「I/O 外壳」的环
（第 7 条），性质最严重：它让 `translate/` 这层不再可能被单独编译/测试，
`models` → `stats` 这条边更是让 Layer 0 依赖了 SQLite 持久层。详见 §1.5。

---

## 1. 逐条复核 7 个环：每条边是否真的必要 + 最小消环改法

### 复核方法（可复现）

不用正则直接扫——那会把注释里的 `crate::x`、`use` 里被换行的路径、字符串里的 URL
误判成依赖。实际做法：

1. **去注释/去字符串**：状态机剥离 `//`、嵌套 `/* */`、`"…"`、`r#"…"#`、char 字面量
   （`'a'` 与生命周期 `'a` 要区分），保留换行以保证行号不偏移。
2. **去测试代码**：定位 `#[cfg(test)]` 属性后的项，按花括号配平整块剔除；
   把 `src/proxy/tests.rs` 整个文件排除（理由见 §0.1）。
3. **只认 `crate::<模块名>` 形式**，模块名必须在 `src/lib.rs` 的 34 个 `pub mod` 里。
   全库没有 `use crate::{a, b}` 形式的嵌套花括号（已 `grep` 确认为 0 命中），
   所以不存在漏边。
4. 结果：生产代码里 **417 个跨模块 `crate::` 站点**（原始文本 594 个，差额 177 个全在注释/字符串/测试里）、去重后 **136 条模块级边**（含重复站点合并）。
   边清单与逐行证据已落盘（见 §6）。

### 总览：消环的代价与收益（已用改图模拟验证）

下表是我**实际删边后重跑环枚举**的结果，不是估计：

| 阶段（编号与 §5 一致） | 改动 | 删掉的边 | 剩余基本环 | **本步真正杀掉哪个环** |
|---|---|---:|---:|---|
| 基线 | — | 0 | **7** | — |
| **P0** | 新增 `env.rs`，4 个 `env_value` 消费点改指向 | 4 | 5 | #2 `client_accounts ↔ config`、#3 `api_token → config → client_accounts` |
| **P1** | 新增 `store.rs`（`StoreState`/`inspect_store_for`） | 1 | 4 | 无独立收益（#6 的一条腿，需等 P3+P4） |
| **P2** | `constant_time_eq` 迁入 `util::crypto` | 1 | **4** | **无——见下方更正** |
| **P3** | 新增 `billing.rs`，积分协议迁出 `credits` | 1 | **2** | #4 `config → wb → credits → config`、#5 `credits → pool → wb → credits`（一次断两环） |
| **P4** | 删 `Config::client_auth_enabled`（死字段） | 1 | 2 | #6（4 环，最后一条腿是 `config → client_accounts`） |
| **P5** | 新增 `admin_state.rs`（`AdminState` 迁出） | 1 | 1 | #1 `admin ↔ admin_routes` |
| （3.4.0）P6-P10 | `TokenRecord`/`FREE_MODEL_NAME` 下沉、`settings`/`util` 拆分 | 4 | **0** | #7 —— **只需其中任意一条边即可断开**（实测：单独断 `models→stats`、`providers→translate`、`stats→settings` 或 `settings→providers`，任一条都让环 7 消失） |

**P0 是收益最高的一步**（4 条边、2 个环、零测试改动），**P3 次之**（1 条边、2 个环）。

### ⚠️ 更正：`constant_time_eq` 那条边在 P0 之后就不再闭合任何环

任务描述把 `api_token -> config -> client_accounts -> api_token` 列为一条环。
实测：这条环**完全由 P0（`env` 模块）杀死**——因为 `api_token → config`（`api_token.rs:42`）
与 `client_accounts → config`（`client_accounts.rs:539`）都只是读环境变量，两条 `→config` 边
同时换成 `→env` 之后，`api_token` 与 `client_accounts` 都到不了 `config` 了，这个三元环自然消失。

删边模拟（只做 `constant_time_eq` 迁移、不做 `env`）：仍有 **6** 个环；
只做 `env`、不做 `constant_time_eq`：**5** 个环（与两者都做相同）。

**所以 P2 不是消环必需，而是一条「方向正确但不紧急」的独立改进**
（`constant_time_eq` 是通用密码学工具，住在 `api_token` 里让 `client_accounts` 为了一个
字节比较而依赖整个 token 校验模块）。**可以延后，不要为了「消环」而先做它。**

---

### 1.1 环 1：`admin ↔ admin_routes`（2 环）

**边复核。**

| 方向 | 证据 | 内容 |
|---|---|---|
| `admin → admin_routes` | `src/admin.rs:790`、`:828`、`:851` | 三个 `async fn` 的参数类型 `&Arc<crate::admin_routes::AdminState>` |
| `admin_routes → admin` | `src/admin_routes.rs:35` | `use crate::admin;` |

**边的必要性：不必要，且方向是反的。**

`admin_routes.rs` 是**传输层**（axum handler + 路由表），`admin.rs` 是**用例层**
（读改 store、拼 JSON payload）。传输层依赖用例层是对的；用例层依赖传输层的
「状态聚合结构体」是错的。

那三个函数是：

| 函数 | 行 | 实际用到 `state` 的哪些字段 |
|---|---|---|
| `apply_dsh_config` | `admin.rs:789-822` | `state.process.bound_port`（`admin.rs:810`） |
| `refresh_all_points` | `admin.rs:826-847` | `state.client`（`:830`）、`state.logs`（`:832`） |
| `refresh_one_points` | `admin.rs:849-877` | `state.client`（`:854`）、`state.logs`（`:857`） |

**它们一个字段都没用到 `AdminState` 的 `sessions` / `stats` / `client_auth` / `pool`。**
拿到整个 `AdminState` 只是为了顺手取 `process` / `client` / `logs` 三个字段——
**过度参数化，不是真的需要这个类型**。

而这 3 个函数唯一的调用者是 `src/admin_rpc.rs:253`、`:256`、`:333`，
它们传的正是 `state: &Arc<AdminState>`。

**最小消环改法（推荐 A2，改动 3 文件、11 行）：**

**A1（最小，1 行级）**——把 `AdminState` 的定义搬到 `admin.rs`（或新 `admin_state.rs`），
`admin_routes.rs` 反过来 `use crate::admin_state::AdminState`。
边变成 `admin_routes → admin`（单向），环消失。

**A2（更对症，推荐）**——按实际所需把参数降维：

```rust
// admin.rs
pub async fn apply_dsh_config(bound_port: u16) -> Result<Value>          // 原 state.process.bound_port
pub async fn refresh_all_points(client: &reqwest::Client, logs: &LogBuffer) -> Result<Value>
pub async fn refresh_one_points(client: &reqwest::Client, logs: &LogBuffer, id: &str) -> Result<Value>
```

调用点同步改成 `admin::apply_dsh_config(state.process.bound_port)` 等 3 处
（`admin_rpc.rs:253`、`:256`、`:333`）。`admin.rs` 对 `admin_routes` 的 3 条边全部消失，
且**这三个函数从此可以在 `admin.rs` 自己的单测里直接调**——不需要构造 5 个 `Arc` 的
`AdminState`。这是收益，不是副作用。

**改动面。**
- 生产：`admin.rs`（3 处签名 + 函数体 3 处字段访问）、`admin_rpc.rs`（3 处调用）。
- 测试：**0**。`tests/admin_api.rs` 只在 `:20` 导入 `AdminState`、在 `:70/:97/:448/:763/:834`
  构造它给 `build_admin_router` 用，不调这三个函数；`admin.rs` 自己的测试（2 个）只测 `mask_key`。
- `AdminState` 的 `pub` 定义与 `build_admin_router` 签名**完全不动**，
  `tests/admin_api.rs` 零改。

**风险点：** `apply_dsh_config` 现在从 `state.process.bound_port` 取端口，如果未来它还要用
`state.client`（复用连接池，`admin_routes.rs:56-59` 的注释说明这是有意的），A2 需要再加一个
`&reqwest::Client` 参数——仍然不引入类型依赖，仍然消环。

---

### 1.2 环 2 + 环 3 + 环 6 的公共结点：`config` 与「环境变量」的错位

这三条环里有 4 条边是同一个病灶的四种表现：

| 边 | 证据 | 那一行到底在干什么 |
|---|---|---|
| `api_token → config` | `src/api_token.rs:42` | `crate::config::env_value(TOKEN_ENV)` — 读一个环境变量 |
| `client_accounts → config` | `src/client_accounts.rs:539` | `match crate::config::env_value(ENABLED_ENV)` — 读一个环境变量 |
| `client_auth → config` | `src/client_auth.rs:191` | `crate::config::env_value("UPSTREAM_API_KEY_PASSTHROUGH")` — 读一个环境变量 |
| `admin → config` | `src/admin.rs:632` | `"env_forced": crate::config::env_value(...).is_some()` — 读一个环境变量 |
| `config → client_accounts` | `src/config.rs:343` | `self.client_auth_enabled = crate::client_accounts::enabled_from(self.client_auth_enabled)` — 一次「开关 + 环境变量覆盖」的折算 |

**这就是本案最大的结构性错位：`.env` 解析器住在 `config.rs` 里，而 `config.rs` 是
「上游配置对象」（`Config` 结构 + URL 解析 + model map），`env_value` 跟它毫无语义关系。**

`config.rs:82-91` 的文档自己承认了这是外借：

> `/// A configuration value from '.env' or the process environment, verbatim.`
> `/// The same '.env'-aware lookup 'Config::from_settings' uses, **exposed for settings read outside the proxy config** — today the `/api` mutation token.`

也就是说：`env_value` 是一个**被临时塞进 config 的通用基础设施**，现在有 4 个模块为了它而
依赖整个 `Config`。同理 `env_lookup` / `non_empty_env` / `env_flag` / `env_u16` / `dotenv_values`
（`config.rs:17`、`:75`、`:94`、`:101`、`:108`）本来是自洽的一簇，但只有 `env_value` 被外借。

**最小消环改法：新建 `src/env.rs`，把整簇环境变量读取搬过去。**

```rust
// src/env.rs —— 新文件，零依赖（只用 std + dotenvy）
pub fn value(name: &str) -> Option<String>        // 原 config::env_value
pub(crate) fn lookup(name: &str) -> Option<String> // 原 config::env_lookup
pub fn non_empty(name: &str) -> Option<String>     // 原 config::non_empty_env
pub fn flag(name: &str) -> bool                    // 原 config::env_flag
pub fn u16_(name: &str) -> Option<u16>             // 原 config::env_u16
fn dotenv_values() -> &'static HashMap<String,String>
```

然后：
- `config.rs` 删掉这 5 个函数 + `dotenv_values`，改成 `use crate::env;`，内部
  `non_empty_env(x)` → `env::non_empty(x)` 等（**21 处调用**，行号：`config.rs:109/233/239/244/245/`
  `298/299/300/306/310/313/316/320/321/327/328/329/333/335`）。
- `api_token.rs:42` → `crate::env::value(TOKEN_ENV)`
- `client_accounts.rs:539` → `crate::env::value(ENABLED_ENV)`
- `client_auth.rs:191` → `crate::env::value(...)`
- `admin.rs:632` → `crate::env::value(...)`

`config.rs` 的 `mod tests`（`config.rs:551` 起，305 行）里与 dotenv 解析相关的测试
（含 `dotenv_values_are_parsed_without_touching_the_process_environment`，`config.rs:565` 的
`#[test]`）随函数一起搬到 `env.rs`——**逻辑一字不改，只是换了宿主文件**。

**改动面。**
- 生产：新增 `env.rs`（约 90 行，从 `config.rs` 平移）、`config.rs`（删 + 17 处内部改名）、
  4 个调用点各 1 行、`lib.rs` 加 1 行 `pub mod env;`。
- 测试：`config.rs` 的 dotenv 测试搬文件；**外部调用方 0 改**（4 个 `env_value` 调用点改
  路径，但都在生产代码里，且是一行）。
- **保留兼容**：`config.rs` 里留一句
  `pub use crate::env::value as env_value;` —— 则 4 个调用点**一行都不用改**，
  而 `config` 的出边照旧消失（因为 re-export 是 `config → env` 方向，不产生 `X → config`）。
  这是 §5 手法清单里的第 1 条。

**这一刀同时消掉 4 条边**，是全部改动里性价比最高的。

---

### 1.3 `config → client_accounts`：一次默认值折算，可以彻底删掉

`src/config.rs:343`：

```rust
self.client_auth_enabled = crate::client_accounts::enabled_from(self.client_auth_enabled);
```

`enabled_from`（`client_accounts.rs:538-546`）的逻辑：

```rust
pub fn enabled_from(settings_flag: bool) -> bool {
    match crate::config::env_value(ENABLED_ENV) {   // ← 它自己也依赖 config!
        Some(raw) => !(raw == "0" || "false" || "no"),
        None => settings_flag,
    }
}
```

这是个**纯函数**：`bool + Option<&str> → bool`。写成现在这样只是为了顺手读环境变量。

**关键发现：`Config::client_auth_enabled` 这个字段在全库是「只写不读」的。**

`grep -rn '\.client_auth_enabled'` 全部命中：

```
src/config.rs:343       写（唯一赋值点，在 apply_env_overrides 里）
src/admin.rs:93         读的是 settings.client_auth_enabled（GuiSettings 的字段，不是 Config 的）
src/admin.rs:216-217    写 GuiSettings 的字段
src/bin/proxy-rs-server.rs:160   读 settings.client_auth_enabled（GuiSettings）
src-tauri/src/main.rs:533/554/2487 读 GuiSettings / 写 GuiSettings
```

**没有任何代码读 `Config::client_auth_enabled`。** 真正决定 inbound gate 的是
`ClientAuthState::new(enabled)`，两处调用者（`src/bin/proxy-rs-server.rs:160`、
`src-tauri/src/main.rs:2487`）都是**自己直接调 `client_accounts::enabled_from(settings.…)`**，
绕过了 `Config`。

所以 `config.rs:343` 是一次**没有消费者的计算**。

**最小消环改法（两条，任选）：**

- **C1（最保守，1 行）**：把 `enabled_from` 的签名改成参数化环境值
  `pub fn enabled_from(settings_flag: bool, env_raw: Option<&str>) -> bool`，
  `config.rs:343` 改成 `crate::client_accounts::enabled_from(self.client_auth_enabled, crate::env::value(client_accounts::ENABLED_ENV).as_deref())`
  —— 边变成 `config → env`，环消失；`enabled_from` 变成纯函数，它的测试（`client_accounts.rs` 的 14 个测试里涉及的部分）可以直接喂值。
- **C2（更彻底，推荐）**：直接**删掉 `config.rs:343` 这一行和 `Config::client_auth_enabled` 字段本身**
  （`config.rs:168` 定义、`:192` Default 初始化）。理由：无人读。
  这一步让 `config → client_accounts` 边**连同字段一起消失**，
  同时让 `Config` 少一个「两套开关的真值只有一处」的陷阱。

**改动面（C2）。**
- 生产：`config.rs` 删 3 行。
- 测试：**1 处**——`tests/client_auth.rs:375`、`:415`、`:473`、`:632` 里有
  `client_auth_enabled: true` 出现在 `Config { … }` 结构体字面量中，删字段后这 4 行要删。
  **注意它们本身就是死配置**（因为字段无人读，这 4 个测试传了也不生效）——删掉是纯粹的减法，
  且不改变任何测试的通过/失败，因为它们依赖的是 `ClientAuthState::new(true)`（同文件 `:369`）。
- 建议：先做 C1（零测试改动），把字段标注 `#[deprecated]`；3.3.x 再做 C2。

---

### 1.4 `client_accounts ↔ workbuddy_auth`（环 6 的边，1 条）

`src/client_accounts.rs:556-558`：

```rust
pub fn store_health() -> crate::workbuddy_auth::StoreState {
    crate::workbuddy_auth::inspect_store_for::<Vec<ClientAccount>>(accounts_path())
}
```

`inspect_store_for`（`workbuddy_auth.rs:243-247`）→ `inspect_store::<T>`（`:250-259`）：

```rust
fn inspect_store<T: DeserializeOwned>(path: PathBuf) -> StoreState {
    match std::fs::read_to_string(&path) {
        Err(e) if e.kind() == NotFound => StoreState::Missing,
        Err(e) => StoreState::Corrupt(format!("读取失败: {e}")),
        Ok(text) => match serde_json::from_str::<T>(&text) {
            Ok(_) => StoreState::Ok,
            Err(e) => StoreState::Corrupt(format!("解析失败: {e}")),
        },
    }
}
```

**这是一个对任意 `T: DeserializeOwned` 完全泛型的、与 WorkBuddy 毫无关系的**「这个 JSON 文件
能不能读/能不能解析」。`workbuddy_auth.rs:240-242` 的注释自己说了：

> `/// 'pub(crate)' because 'client_accounts.rs' describes its own file through the same rule: the export path iterates one health list, and a second copy of this logic would let "corrupt" mean two different things in two places.`

**写这段注释的人已经知道它是共享基础设施，只是当时没地方放，就放在了 workbuddy_auth 里。**

**最小消环改法：新建 `src/store.rs`，搬 `StoreState` + `inspect_store_for` + `inspect_store`。**

```rust
// src/store.rs —— 新文件，只依赖 serde_json + std::fs
pub enum StoreState { Missing, Ok, Corrupt(String) }
impl StoreState { pub fn is_corrupt(&self) -> bool { … } }
pub fn inspect_store_for<T: DeserializeOwned>(path: PathBuf) -> StoreState { … }
pub(crate) fn inspect_store<T: DeserializeOwned>(path: PathBuf) -> StoreState { … }
```

调用点改 3 处：
- `client_accounts.rs:556-558` → `crate::store::inspect_store_for::<Vec<ClientAccount>>(...)`
- `client_accounts.rs:846`（测试里 `crate::workbuddy_auth::StoreState::Corrupt(_)`）→ `crate::store::StoreState`
- `pool_transfer.rs` 对 `StoreState` 的 2 处引用 → `crate::store::StoreState`
- `workbuddy_auth.rs:265` 的 `store_health()` 内部改用 `store::inspect_store`，
  并在 `workbuddy_auth.rs` 里留 `pub use crate::store::{StoreState, inspect_store_for};`
  让 `crate::workbuddy_auth::StoreState` 这条老路径继续能解析。

**改动面。**
- 生产：新增 `store.rs`（约 45 行平移）、`lib.rs` 1 行、`workbuddy_auth.rs` 删 40 行 + 1 行 re-export、
  `client_accounts.rs` 1 处、`pool_transfer.rs` 4 处（`pool_transfer.rs:325/326/347/348`）。
- 测试：`client_accounts.rs:846` 1 处（且若保留 `pub use`，**这处也不用改**）。
  外部 `tests/` 无引用（`grep proxy_rs::workbuddy_auth` 只有 `WorkBuddyCredential` /
  `WorkBuddyAccount` 两种类型，`tests/client_auth.rs`、`tests/pool_transfer_roundtrip.rs`）。

---

### 1.5 环 7：`models → stats → settings → providers → translate → models`（5 环，未在清单里）

这是**唯一跨「纯核心 / I/O 外壳」边界的环**，也是最该修但任务描述没提的。五条边逐条看：

| 边 | 证据 | 内容 | 必要性 |
|---|---|---|---|
| `models → stats` | `src/models/openai.rs:278-284` | `to_token_record(&self) -> crate::stats::TokenRecord` | **不必要**，见下 |
| `stats → settings` | `src/stats.rs:9` + `:720` | `use crate::settings::data_dir;` 然后 `data_dir().join("stats.db")` | **不必要**，一行路径拼接 |
| `settings → providers` | `src/settings.rs:588/596/606/608/611/612` | `chat_url` / `force_stream` / `models_preset` 三个方法都吃 `&[ProviderPreset]` | **不必要**，见下 |
| `providers → translate` | `src/providers.rs:340`、`:343`、`:597`、`:600` | 只为读一个常量 `crate::translate::pipeline::FREE_MODEL_NAME`（`pipeline.rs:248`，值就是 `"free"`） | **不必要，最刺眼** |
| `translate → models` | `translate/core.rs:2`、`stream.rs:1,5`、`responses.rs:2`、`pipeline.rs:2` | 类型契约 | **必要**（这是 L1/L2 → L0 的正向依赖） |

#### 边 `providers → translate`：为了一个字符串常量，把 L3 拉进 L2

`providers.rs:336-344` 与 `:593-601` 两处的意图是「把虚拟模型 `free` 加进模型列表，若还没有」：

```rust
if !models.iter().any(|m| m.id == crate::translate::pipeline::FREE_MODEL_NAME) {
    models.push(GuiModel { id: crate::translate::pipeline::FREE_MODEL_NAME.to_string(), … });
}
```

`FREE_MODEL_NAME` 定义在 `translate/pipeline.rs:248`：`pub const FREE_MODEL_NAME: &str = "free";`

它被 3 处用：`providers.rs`（2 类站点）、`proxy.rs`（4 站点）、`pipeline.rs` 自己（10 站点）。
其中 `proxy.rs:847`、`:1635`、`:2365`、`:2367` 用的也是 `crate::translate::pipeline::FREE_MODEL_NAME`。

**最小消环改法：把 `FREE_MODEL_NAME` 下沉到 `models`（L0）。**
`models/mod.rs` 目前只有 3 行（`pub mod anthropic; pub mod openai; pub mod responses;`），
加一个 `pub const FREE_MODEL_NAME: &str = "free";` 即可。
`pipeline.rs` 留 `pub use crate::models::FREE_MODEL_NAME;`，则
`providers.rs` 与 `proxy.rs` **一行都不用改**（`crate::translate::pipeline::FREE_MODEL_NAME` 继续解析），
而 `providers → translate` 边消失。

#### 边 `settings → providers`：策略函数放错了宿主

`settings.rs:586-620` 的三个方法 `chat_url` / `force_stream` / `models_preset` 全部以
`&[crate::providers::ProviderPreset]` 为参数，其中 `models_preset`（`:611-619`）甚至自己
`crate::providers::builtin_presets()` 取全局。

它们是**「`GuiSettings` × `ProviderPreset` → 上游语义」的策略函数**，宿主在 `GuiSettings` 上
只因为它要读 `self.provider_id` / `self.custom_url` / `self.force_stream` 三个字段。

**最小消环改法：把它们搬到 `providers.rs` 作为自由函数（参数化 `&GuiSettings`）。**

```rust
// providers.rs
pub fn chat_url(s: &GuiSettings, presets: &[ProviderPreset]) -> String
pub fn force_stream(s: &GuiSettings, presets: &[ProviderPreset]) -> bool
pub fn models_preset(s: &GuiSettings) -> ProviderPreset
```

但 `providers.rs` 若 `use crate::settings::GuiSettings`，就变成 `providers → settings`——
**这是允许的**（`settings` 是结构/路径层，`providers` 是业务层，方向反过来才对），
而且**不产生环**，因为 `settings` 从此不再依赖 `providers`。

不过这需要改 12 个调用点（下表），代价比前几条大：

| 调用点 | 现在 | 改后 |
|---|---|---|
| `config.rs:206` | `settings.chat_url(&crate::providers::builtin_presets())` | `providers::chat_url(settings, &providers::builtin_presets())` |
| `config.rs:209` | `settings.models_preset()` | `providers::models_preset(settings)` |
| `config.rs:224` | `settings.force_stream(...)` | `providers::force_stream(settings, ...)` |
| `admin.rs:78`, `:127`, `:136`, `:140`, `:799` | 同上 | 同上 |
| `admin_routes.rs:473` | `settings.models_preset()` | 同上 |
| `admin_rpc.rs:423`, `:436`, `:587` | 同上 | 同上 |
| `src-tauri/src/main.rs:105`, `:448`, `:538`, `:1474`, `:1609`, `:1732`, `:1800` | 同上（**跨 crate**） | 同上 |

**更省的替代方案（推荐先用这个）**：保留方法签名，让 `settings.rs` **不再直接命名
`providers::ProviderPreset`**，而是把 `providers` 提供的两个**自由函数**改造成吃基本类型：

- `preset_force_stream(presets, provider_id)`（`providers.rs:41`）已经是自由函数，只需把
  `chat_url` / `models_preset` 的「查表」逻辑也做成 `providers::chat_url_for(presets, provider_id, custom_url) -> String`
  与 `providers::preset_for(presets, provider_id) -> ProviderPreset`。
  `settings.rs` 依然要 `use crate::providers`——**这治不了环**。

所以：**`settings → providers` 这条边只能靠「把策略搬出 settings」来消**，做不到零调用点改动。
按 §2 的优先级，它属于 P5-P6，排在最后。

#### 边 `stats → settings`：一行路径拼接

`src/stats.rs:720`：`let path = match data_dir() { Some(dir) => dir.join("stats.db"), … }`。

`data_dir()`（`settings.rs:241`）本身也是一行 `env_data_dir().or(home_dir().join(".proxy-rs"))`。
`stats` 为了它依赖 `settings` 整个模块。

**最小消环改法**：两种都行。

- **S1（零测试改动，推荐）**：`StatsDb::open()` 改成 `StatsDb::open_at(&paths::stats_db_path())`，
  路径由 `settings::stats_db_path()` 提供——**边不消失**（`stats → settings` 还在），所以这条不算消环，
  只是把「stats 知道文件名」变成「settings 知道文件名」。**治不了环，不推荐单独做。**
- **S2**：把路径解析整体下沉成 `paths` 模块（§2.1 的 `settings/paths.rs`），`stats` 改依赖 `paths`。
  `settings` 也依赖 `paths`。边变成 `stats → paths`、`settings → paths`，
  `paths` 无出边 → **`stats → settings` 消失，环断一处**。

这就是为什么 §2.1 的拆分不是「为了好看」：**`settings` 拆成 `paths`/`logbuf`/`gui` 后，
`stats → settings` 与 `settings → providers` 两条环边会自动降级成「→ 更小、更中立的模块」**。

#### 边 `models → stats`：L0 依赖 SQLite 持久层

`models/openai.rs:278-284`：

```rust
pub fn to_token_record(&self) -> crate::stats::TokenRecord {
    crate::stats::TokenRecord { input: …, cache_read: …, cache_write: …, output: … }
}
```

`TokenRecord`（`stats.rs:12-31`）是**纯数据**：4 个 `i64` 字段 + `#[derive(Debug, Clone, Default)]`。
它住在 `stats.rs`（1528 行生产代码、含 `rusqlite`）纯粹是历史原因。

**最小消环改法：把 `TokenRecord` 搬到 `models/tokens.rs`（或 `models/mod.rs`）。**
`stats.rs` 留 `pub use crate::models::tokens::TokenRecord;`
→ `stats → models`（正向，L2→L0）+ `models → stats` 边消失。
调用点 `models/openai.rs:278-279` 改 `crate::models::tokens::TokenRecord`（同模块，可写 `super::tokens::TokenRecord`）。
`proxy.rs:43`、`api.rs:44`、`lib.rs:45`（`pub use stats::{DayStats, StatsDb, TokenRecord};`）继续解析。

**改动面**：生产 3 行；测试 0 行（`proxy/tests.rs` 用的 `TokenRecord` 走 `crate::stats::TokenRecord` 或
`use crate::stats::{…}`，re-export 保留即不动）。

---

### 1.6 环 4 + 环 5 的公共结点：`workbuddy_auth → credits` 的积分协议

| 边 | 证据 | 内容 |
|---|---|---|
| `workbuddy_auth → credits` | `workbuddy_auth.rs:1992`、`:2011`、`:2040`、`:2059`、`:2080`、`:2099` | `WORKBUDDY_RESOURCE_PATH` / `WORKBUDDY_RESOURCE_PRODUCT_CODE` / `remaining_from_response` |
| `credits → config` | `credits.rs:18`、`:31` | `use crate::config::Config;` / `use crate::config::ModelsFlavor;` |
| `credits → session_pool` | `credits.rs:681` | handler 的 `Extension<Arc<SharedCredentialPool>>` |
| `session_pool → workbuddy_auth` | `session_pool.rs:30`、`:425` | `WorkBuddyCredential` 类型 + `ordered_credentials()` |

**边的必要性：三条里两条是真需要，一条是为了复用 4 个符号。**

- `credits → config`：`credits_endpoint_for(config: &Config)`（`credits.rs:158-167`）真读
  `config.credits_endpoint` 与 `config.models_flavor`。**真需要。**
- `credits → session_pool`：handler 要读池里默认凭据的 access token。**真需要**（可以改成参数化
  一个 `Option<String>`，但那是 handler 层的事）。
- `session_pool → workbuddy_auth`：`WorkBuddyCredential` 是池的元素类型。**真需要**，
  除非把 `WorkBuddyCredential` 也下沉（不划算）。
- **`workbuddy_auth → credits`：不必要。** 见下。

`workbuddy_auth` 要的是「积分/账单协议」这一小块：两个常量 + 一个纯聚合函数
（`remaining_from_response`，`credits.rs:213-230`，签名 `&Value -> f64`，纯函数）。
而 `credits.rs` 的其余 700 行生产代码是 **handler + 端点选择 + 单据解析**，跟 `workbuddy_auth` 无关。

**最小消环改法：新建 `src/billing.rs`，搬「协议层」三件套。**

```rust
// src/billing.rs —— 只依赖 serde_json；纯函数 + 常量，无 I/O、无 Config
pub const WORKBUDDY_BILLING_HOST: &str = "https://www.codebuddy.cn";
pub const WORKBUDDY_RESOURCE_PATH: &str = "/v2/billing/meter/get-user-resource";
pub const WORKBUDDY_RESOURCE_PRODUCT_CODE: &str = "p_tcaca";
pub fn remaining_from_response(value: &Value) -> f64
// 以及它依赖的 collect_accounts / first_number / parse_workbuddy / merge_segments / SUMMARY_REMAINING_FIELDS
```

`credits.rs` 留 `pub use crate::billing::{…};` → 现有 5 个常量的引用
（`credits.rs:168`、`:186`、`:521`，`WORKBUDDY_BILLING_HOST` 另有 `:167` 的 `use` 与 `:521` 的比较）与外部引用全部继续解析。
`workbuddy_auth.rs` 的 6 处改成 `crate::billing::…`（或靠 re-export **一处不改**）。

**这一刀砍掉 2 条环（环 4、环 5 同时消失），是继 `env.rs` 之后性价比第二的。**

**改动面。**
- 生产：新增 `billing.rs`（约 180 行平移）、`credits.rs` 删 + 1 行 re-export、`lib.rs` 1 行、
  `workbuddy_auth.rs` 6 处路径（可零改）。
- 测试：`credits.rs` 共 4 个测试（`#[test]` ×2 在 `:751`、`:784`；`#[tokio::test]` ×2 在 `:792`、`:811`），
  全在 `credits.rs:746` 起的 `mod tests` 内。它们测的是 `parse_workbuddy` / `merge_segments` /
  `extract_generic_credits` / `fetch_credits`——**前两个（`:751`、`:784`）随协议层搬到 `billing.rs`，
  后两个（`:792`、`:811`，都要构造 `Config` 测端点失败路径）留在 `credits.rs`**。
  `remaining_from_response`（`:213`）本身目前**没有测试直接覆盖**（`grep -n remaining_from_response src/credits.rs`
  只命中定义处），所以它是纯粹的搬移。测 `parse_workbuddy`/`merge_segments` 的两个测试跟着走，断言不改。
- 外部：`src-tauri/src/main.rs` 不引用 `credits`（`grep proxy_rs::credits` 为 0），零影响。

---

## 2. 环以外的高耦合点：三个 god module 的按职责切分

### 2.0 扇入的正确口径

任务描述给的 `settings` 18 / `util` 16 与我实测略有差异，因为口径不同。三种口径都在下面列出，
**用哪个口径看你在回答什么问题**：

| 模块 | 消费方**模块**数<br>（去测试代码） | 消费**文件**数<br>（库内） | 跨 crate 消费 |
|---|---:|---:|---|
| `settings` | **19** | 21 | 是（`src-tauri`） |
| `util` | **18** | 20 | 是（`src-tauri`） |
| `workbuddy_auth` | **11** | 14 | 是（`src-tauri`，**64 站点**，最重） |
| `client_accounts` | 11 | 11 | 是（`src-tauri`，11 站点） |
| `providers` | 10 | 11 | 否 |
| `stats` | 7 | 7 | 是 |
| `config` | 6 | 6 | 否 |

（`settings` 的 19 个消费方：admin, admin_routes, admin_rpc, api, claude_config, client_accounts,
codex_config, config, console, credits, dsh_config, launch_agent, pool_transfer, proxy, router,
scheduler, scheduler_state, stats, workbuddy_auth）

**注意 `workbuddy_auth` 的跨 crate 依赖是 64 个站点**——比 `settings` 与 `util` 的跨 crate 消费
（各 2-3 站点：`LogBuffer`/`log_dir`/`unix_millis`）重得多。
这意味着 `workbuddy_auth` 的拆分**不能只考虑库内边界，还要考虑 `src-tauri` 的兼容**。

---

### 2.1 `settings.rs`（生产 557 行 / 扇入 19）：四件事，拆 4 个子文件

**四个职责，彼此没有语义关系，只是都被「设置」这个词收留了：**

| # | 职责 | 行区间 | 生产行 | 对外符号 | 真实消费方 |
|---|---|---|---:|---|---:|
| S1 | 环形日志缓冲 + 落盘镜像 | 9-115 | 99 | `LogBuffer`, `LogEntry` | **19 站点 / 6 模块**（admin_routes, api, credits, proxy, router, scheduler）+ 9 个集成测试文件 |
| S2 | 日志文件单写线程 + 文件路径 | 303-551 | 199 | `log_dir`, `log_file_path`, `LogInstruction`, 单写线程, `set_log_file_override_for_tests` | 1 模块（`launch_agent`）+ `LogBuffer` 自己 |
| S3 | 路径解析 | 205-302 | 86 | `home_dir`, `data_dir`, `settings_path`, `dotenv_path`, `DATA_DIR_ENV`, `LEGACY_DATA_DIR_ENV`, `data_dir_test_lock` | **17 模块**（几乎全库；`home_dir` 6 模块 + `data_dir` 7 模块，另有测试锁与 `DATA_DIR_ENV` 的测试消费方） |
| S4 | `GuiSettings` 结构 + 存取 + 三个策略方法 | 120-204, 552-621 | 83+65+15+3=166 | `GuiSettings`, `DEFAULT_PORT`, `chat_url`, `force_stream`, `models_preset`, `normalize_chat_url`(15行) | **7 模块 / 18 站点**（其中策略方法 10 站点） |

**关键观察：扇入不是均匀分布的。**

- `LogBuffer` 一个类型 **19 站点 / 6 模块**，是 `settings` 扇入的主要来源；
- `home_dir` **6 站点 / 6 模块**（claude_config, codex_config, config, dsh_config, launch_agent,
  pool_transfer），`data_dir` **12 站点 / 7 模块**（admin, bin, client_accounts, pool_transfer,
  scheduler_state, stats, workbuddy_auth）——铺得很开但每处只是一行路径计算；
- `GuiSettings` **18 站点 / 7 模块**，但绝大多数是 `GuiSettings::load()` 一行；
  **真正的问题是它的三个策略方法**（`chat_url`/`force_stream`/`models_preset`，共 10 站点）
  才是 `settings → providers` 那条环边的来源。

**切分建议（目录形态，沿用 `src/proxy/` 的既有先例）：**

```
src/settings/
  mod.rs        # pub use 兼容层（§5），约 30 行
  logbuf.rs     # S1: LogEntry + LogBuffer                 ~99 行  扇入 6 模块/19 站点
  logfile.rs    # S2: 单写线程 + log_dir/log_file_path     ~199 行 扇入 1 模块
  paths.rs      # S3: home/data/settings/dotenv/测试锁      ~86 行  扇入 17 模块
  gui.rs        # S4: GuiSettings + load/save + 策略方法 + DEFAULT_PORT + normalize ~166 行 扇入 9 模块
```

切分后各子模块扇入预估（按 §2.0 口径，去测试代码）：

| 子模块 | 消费方模块数 | 说明 |
|---|---:|---|
| `settings::logbuf` | **6** | admin_routes, api, credits, proxy, router, scheduler |
| `settings::paths` | **17** | 全库最广，但**每个消费方只用 1-2 行**（`data_dir()` / `home_dir()`） |
| `settings::gui` | **7** | admin, admin_routes, admin_rpc, config, console, bin(`proxy-rs-server`), lib；另 `src-tauri/src/main.rs` 跨 crate 消费 |
| `settings::logfile` | **1** | 仅 `launch_agent` 用 `log_dir`/`log_file_path` |

**切完 `settings → providers` 边自动消失**（策略方法跟着 S4 走，但 S4 若继续保持对 `providers`
的依赖，环还在）。所以 **S4 必须做两件事之一**：把策略方法搬去 `providers`（§1.5），
或者让 `gui.rs` 依赖 `providers` 而**没有任何模块依赖 `gui.rs` 的 `providers` 相关面**——
前者才是消环，后者只是把环缩小。

`paths.rs` 扇入 17 是「健康的宽耦合」：一个只做 `PathBuf` 计算的模块，17 个消费方各取所需，
改它的风险由它的**纯函数性**兜底（无状态、无 I/O、只读环境变量）。**不要试图降低这个数字**，
那是过度设计。

**测试影响。** `settings.rs` 内联测试 359 行 / 17 个测试，按职责跟走：
`LogBuffer` 的测试（`settings.rs:974` 的 `log_buffer_trims_to_capacity` 与
`:880`、`:943`、`:978` 三处 `LogBuffer::new` 构造，含容量/持久化/清空语义）→ `logbuf.rs`；
`home_dir` 的 4 个跨平台测试（`settings.rs:645` 起的 `#[test]`，含那个「Windows 上 HOME 未设」的
回归测试 `home_dir_prefers_home`，`settings.rs:646`，其 `remove_var("HOME")` 在 `:668`）→ `paths.rs`；
GUI 设置往返测试 → `gui.rs`。**测试代码 1:1 搬迁，断言零改动**，因为 `use super::*` 在原文件内
仍能解析到 `mod.rs` 的 re-export。

**这是全部拆分里最安全的一个**：`settings` 的 4 个子职责之间**没有任何互相调用**
（S1 调 S2 的 `append_log_line`，S3 是所有人的基础，S4 只调 S3 的 `settings_path`）。
唯一的交叉是 `LogBuffer::push` → `append_log_line`(S2) 与 `LogBuffer::load_persisted` → `log_file_path`(S2)，
即 S1 → S2 单向，方向正确。

---

### 2.2 `util.rs`（生产 428 行 / 扇入 18）：四件事，拆 4 个子文件 + 摘掉一条出边

**四个职责（彼此零关联）：**

| # | 职责 | 行区间 | 生产行 | 对外符号 | 真实消费方 |
|---|---|---|---:|---|---:|
| U1 | 文本裁剪 | 13-41 | 28 | `truncate` | **5 模块 / 13 站点**（credits, growth, providers, proxy 含 2 个子文件, workbuddy_auth） |
| U2 | HTTP 头脱敏 | 42-144 | 95 | `format_headers` + `SENSITIVE_HEADERS` 表 + `is_sensitive_header` + `redact_header_value` | **2 模块 / 5 站点**（`proxy.rs:135/309/478/2258`、`credits.rs:688`；**且被 `tests/router_redaction.rs` 专门测**） |
| U3 | 请求体缓冲与限制 | 145-303 | 147 | `DEFAULT_MAX_BODY_BYTES`, `MAX_BODY_ENV`, `max_body_bytes`, `human_bytes`, `PeekedBody`, `peek_json_body`, `rejection_error` | 2 模块（proxy, router）/ **9 处引用**（`proxy.rs:117/118/291/292/460/461/634/662`、`router.rs:60`）+ `tests/body_buffer_limit.rs` |
| U4 | 日期/时区计算 | 304-462 | 148 | `civil_from_days`, `format_epoch_secs`, `local_utc_offset_secs`, `unix_millis`, `ms_until_local_midnight`, `local_datetime_millis`, `days_from_civil` | **18 模块 / 82 站点** |

**关键观察：`unix_millis` 一个函数 49 站点 / 13 模块，占了 `util` 扇入的大半。**
它是 `SystemTime::now()` 的一行封装。把它留在 `util`（或迁到 `time`）不影响任何架构问题——
**它是「名义耦合」，不是「实质耦合」**。真正的问题是 U4 有 3 处生产代码的 `std::env` 读取（`local_utc_offset_secs` 读
`PROXY_TZ_OFFSET_HOURS`，其余 6 处 `std::env` 在测试里），这违反 README「纯函数核心禁 I/O」的约定——但 `util` 不在纯核心里，
所以这是**文档债而非架构债**，只有 `translate/pipeline.rs:315-316` 一处把 `util` 拉进了纯核心
（见 §3.3）。

**切分建议：**

```
src/util/
  mod.rs      # pub use 兼容层
  text.rs     # U1: truncate + human_bytes                    ~38 行  扇入 8 模块
  redact.rs   # U2: 头脱敏（唯一被集成测试专门覆盖的组）        ~95 行  扇入 4（含 tests/）
  body.rs     # U3: 请求体上限与缓冲（axum 相关）              ~147 行 扇入 4（含 tests/）
  time.rs     # U4: 日期/时区/unix_millis                     ~148 行 扇入 18
```

切分后扇入预估：`text` **8**、`redact` **4**（proxy, credits, util, tests/router_redaction.rs）、
`body` **4**（proxy, router, util, tests/body_buffer_limit.rs）、`time` **18**。

**`time` 扇入仍是 18，这是对的，不要动。** 判断标准不是「扇入数字大小」，而是
「这个模块的语义是否单一、它的消费方是否只取它该取的东西」。`time` 只提供时间函数，
18 个消费方各要一个时间函数，这是**正常的基础设施扇入**；相比之下 `settings` 的 19
是「19 个模块为了 5 件不同的事来找它」，那才是不正常。

**顺带修一条出边：`util → error`（`util.rs:286-289`）。**

`rejection_error(route, JsonRejection) -> crate::error::ProxyError` 是 `util` 唯一依赖
`error` 的地方，也是 `util` 唯一依赖 axum `extract::rejection` 的地方。它**本质是 U3（body）**
的一部分：把 axum 的 `JsonRejection` 翻成本项目的 `ProxyError`。

切到 `util/body.rs` 后，`util → error` 边仍在（`body → error`），但**`util` 顶层（`text`/`time`）
不再依赖 `error`**——这很重要，因为 `error.rs` 依赖 `reqwest`（`error.rs:65` `Http(#[from] reqwest::Error)`），
而现在任何想用 `truncate()` 的模块都被迫把 `reqwest`/`axum` 拖进编译单元。

**测试影响。** `util.rs` 内联 229 行 / 18 个 `#[test]`，按组跟走；`tests/router_redaction.rs`
（2 个测试）与 `tests/body_buffer_limit.rs`（7 个测试）调的是 `proxy_rs::util::…` 或经过 router，
**只要 `util/mod.rs` 保留 re-export 就不用改**。

---

### 2.3 `workbuddy_auth.rs`（生产 2515 行 / 扇入 11 / 跨 crate 64 站点）：八件事，切八刀

**这是全库最大的 god module，也是唯一横跨「桌面端 + 服务器端」两边 64 个调用点的模块。**

八个职责（非重叠区间，实测行数）：

| # | 职责 | 行区间 | 生产行 | 对外符号（节选） | 消费方 |
|---|---|---|---:|---|---:|
| W1 | 凭据/账号类型 + id 派生 + 脱敏 + MD5 | 20-685 | 631 | `WorkBuddyCredential`, `WorkBuddyAccount`, `identity_key`, `credential_id_for`, `mask_secret`, `md5_hex`, `UNKNOWN_EXPIRY_REFRESH_AFTER_MS` | **6 模块 / 69 站点**（admin, growth, growth_runner, pool_transfer, proxy, session_pool）+ `src-tauri` |
| W2 | 登录态解析 | 686-765 | 73 | `parse_login_state` | **1 模块 / 2 站点**（`admin.rs:336`、`:423`，都是「粘贴登录态」路径）+ `src-tauri` |
| W3 | API key 池（类型 + 存取 + 默认选择） | 766-1224 | 433 | `ApiKeyEntry`, `load_api_keys`, `save_api_keys`, `active_api_key`, `active_api_key_entry`, `enabled_api_keys`, `set_default_api_key`, `api_key_points` | **6 模块 / 38 站点**（admin, admin_rpc, bin, config, pool_transfer, proxy）+ `src-tauri` |
| W4 | 账号池存取 + 选择策略 | 1225-1389 | 153 | `load_credentials`, `save_credentials`, `upsert_credential`, `migrate_credential_ids`, `ordered_credentials`, `PoolPreferences`, `load_preferences`, `save_preferences`, `save_schedule`, `set_default_identity`, `resolve_default_identity`, `NO_ACCOUNT_POOL_SENTINEL`, `reorder_pin_target`, `set_points`, `display_label` | **9 模块 / 63 站点**（admin, admin_routes, admin_rpc, bin, config, growth_runner, pool_transfer, scheduler, session_pool）+ `src-tauri` |
| W5 | token 刷新（串行锁 + 指纹头） | 1390-1602 | 200 | `refresh_credential`, `upstream_headers`, `refresh_locks`, `refresh_failure_needs_relogin`, `mark_credential_refresh_failure`, `refresh_due_credentials`, `AUTH_PREFIX`, `DEFAULT_WORKBUDDY_ENDPOINT` | **2 模块 / 6 站点**��`proxy`、`scheduler`）+ `src-tauri` |
| W6 | OAuth 设备码 / 二维码 | 1603-1941 | 308 | `start_oauth_flow`, `poll_oauth_token`, `generate_qr_svg`, `OAuthPollResult`, `OAuthStateResponse`, `fetch_account`, `finish_oauth_login` | **库内 0 模块 / 0 站点**——只有 `src-tauri` 用（服务器端明确不支持，见 `admin_rpc.rs:260-262`） |
| W7 | 积分查询 + 批量刷新 | 1942-2373 | 398 | `fetch_points`, `fetch_points_for_key`, `refresh_all_points`, `refresh_one_points`, `PointsReport`, `PointsResult`, `RefreshReport`, `RefreshResult` | **2 模块 / 6 站点**（`admin`、`admin_rpc`）+ `src-tauri` |
| W8 | 每日打卡 | 2374-2705 | 301 | `claim_daily_checkin`, `claim_daily_checkin_forced`, `batch_claim_daily_checkin(_forced)`, `CheckinResult`, `is_already_checked_in`, `has_reward`, `local_day`, `millis_until_daily_time`, `normalize_checkin_time` | **5 模块 / 13 站点**（admin, admin_routes, admin_rpc, pool_transfer, scheduler）+ `src-tauri` |

**切分建议：**

```
src/workbuddy_auth/
  mod.rs        # pub use 兼容层（关键：src-tauri 64 站点靠它零改）
  credential.rs # W1: 类型 + id/脱敏/md5              ~631 行
  parse.rs      # W2: parse_login_state               ~73 行
  api_keys.rs   # W3: API key 池                      ~433 行
  store.rs      # W4: 账号池存取 + 选择策略            ~153 行
  refresh.rs    # W5: token 刷新 + 指纹头              ~200 行
  oauth.rs      # W6: 设备码/二维码                   ~308 行
  points.rs     # W7: 积分（依赖 crate::billing）      ~398 行
  checkin.rs    # W8: 打卡                            ~301 行
```

切分后扇入（实测，去测试代码；`+ tauri` 表示另有 `src-tauri` 消费）：

| 子模块 | 库内消费方 | 备注 |
|---|---|---|
| `workbuddy_auth::store` | **9 模块 / 63 站点** | admin, admin_routes, admin_rpc, bin, config, growth_runner, pool_transfer, scheduler, session_pool + tauri |
| `workbuddy_auth::credential` | **6 模块 / 69 站点** | admin, growth, growth_runner, pool_transfer, proxy, session_pool + tauri。纯类型，站点多是因为它出现在每个函数签名里 |
| `workbuddy_auth::api_keys` | **6 模块 / 38 站点** | admin, admin_rpc, bin, config, pool_transfer, proxy + tauri |
| `workbuddy_auth::checkin` | **5 模块 / 13 站点** | admin, admin_routes, admin_rpc, pool_transfer, scheduler + tauri |
| `workbuddy_auth::refresh` | **2 模块 / 6 站点** | proxy, scheduler + tauri。**内聚最高的一刀** |
| `workbuddy_auth::points` | **2 模块 / 6 站点** | admin, admin_rpc + tauri。依赖新 `billing` |
| `workbuddy_auth::parse` | **1 模块 / 2 站点** | admin（`admin.rs:336`、`:423`）+ tauri |
| `workbuddy_auth::oauth` | **库内 0 模块** | 只有 `src-tauri` 用 |

**两个高价值发现：**

1. **`oauth` 在库内是 0 个消费方，`refresh`/`points`/`parse` 各只有 1-2 个。**
   W2 + W5-W8 合计 **1280 行**生产代码，服务的是 **0-5 个库内模块**；而 W1-W4 合计 **1290 行**
   服务的是 **6-9 个模块**。两半的行数几乎相等，但扇入差一个数量级。**「扇入」和「行数」在 `workbuddy_auth.rs` 里是错配的**——
   这就是它成为 god module 的数学原因。
2. **`oauth` 是桌面端独有的**（`admin_rpc.rs:260-262` 明确对服务器端返回
   「服务器模式不支持扫码登录：浏览器回调只能指向本机 localhost」）。
   它的 308 行生产代码 + 依赖 `qrcode`（`Cargo.toml:74`）完全可以只在
   `src-tauri` 编译，库侧用 `#[cfg(feature = "desktop")]` 或干脆移到
   `src-tauri/src/oauth.rs`。**这一刀能把 `qrcode` 从 headless 二进制的依赖里摘掉**——
   对 release profile（`opt-level="z"` + `lto` + `strip`，`Cargo.toml:94-99`）是可减体积的
   （具体减多少需实测 `cargo bloat`/二进制体积，本审计未测，**不作为定量承诺**）。

**测试影响（本文件最大的风险点）。**
`workbuddy_auth.rs` 内联 1040 行测试 / 51 个 `#[test]`，其中 W1 的 `mask_secret` 测试在
`workbuddy_auth.rs:2958-2981`（`2958` 是 `#[test]`，`:2960/:2961/:2962/:2964/:2974/:2980` 共 6 处断言），
W4 的账号池测试占用最多。
**拆分后这些测试跟文件走，但因为所有测试都写的是 `use super::*`，
`mod.rs` 的 `pub use` 会让 `super::*` 解析到同一批符号——测试代码零改动。**
这是 §5 手法的第 2 条，也是本文件最依赖的兼容层。

---

### 2.4 `stats.rs`（生产 1528 行）：顺带一提，不必拆

任务描述把 `stats.rs` 列为 1639 行的 god module，但它**没有环**（出了 `models→stats` 那条，
方向是别人依赖它），且结构本身是清晰的：11 个数据类型的区段 + 1 个 `StatsDb`（767 行 impl）。

**建议：只做 §1.5 的「`TokenRecord` 下沉到 models」一刀，其余不动。**
理由：`StatsDb` 的 767 行是 SQLite 的 schema/migrate/query，是**一个真实的内聚单元**
（所有方法都操作同一张 `request_logs` 表）。按「查询/写入」硬切会造出两个都要访问同一个
`Connection` 池的模块，反而增加耦合。

这也说明本审计的判据不是「行数」，而是**「是否有多件事被塞进一个命名」**。
`stats.rs` 只有一件事（请求日志的持久化与查询）；`workbuddy_auth.rs` 有八件。

---

## 3. 目标分层

### 3.1 现状：README 承诺 L0-L3，实测有 3 处越界

README（`README.md:359-361` 的「四层函数式分层」+ `README.md:365-395` 的逐文件分层清单 +
`README.md:402` 的「纯函数核心」约定）承诺四层分层；`specs/architecture.md:43` 明确写着
「LAYER 0: Protocol Types → **Depends on: Nothing**」。实测越界情况：

| 层级 | README 声称 | 实测越界 | 证据 |
|---|---|---|---|
| L0 `models/` | 无依赖 | 依赖 `stats`（SQLite 持久层） | `models/openai.rs:278` |
| L1/L2 `translate/` | 无 I/O、无 async、无 logging（`README.md:402`） | 依赖 `util`（含 `std::env`） | `translate/pipeline.rs:315-316` |
| | | 自己直接读环境变量（生产代码 6 处 `std::env::var`） | `translate/pipeline.rs:262/264/265/269/286/294` |
| L3 各模块 | I/O 外壳 | 反向依赖传输层结构体 | `admin.rs:790/828/851`（`admin → admin_routes`） |

其中 **L1/L2 的环境变量读取（10 处）是文档里没承认的自相矛盾**：
`specs/architecture.md:91` 说 LAYER 2a「Depends on: Layer 1, Layer 0」，
但 `pipeline.rs:262` 的 `std::env::var("PROXY_FREE_DAY_MODEL")` 让 L2 依赖了进程环境。
`pipeline.rs:300-301` 的注释倒是自洽的（「Kept pure (no clock access) so it can be unit-tested
deterministically」——`is_night_hour` 确实纯），但**它的调用者 `is_night`（`:314-319`）
不纯，且 `resolve_upstream_model` 经 `is_night` 间接受影响**。

**这不是本任务要修的**（它不产生环），但**它决定了 `translate` 不能被当作真正的纯层**，
所以下面 §3.2 把它标为 L2'。

### 3.2 目标分层（5 层，沿用 README 词汇 + 显式拆出「契约」与「中立基础设施」）

```
L0  契约层 (contract)       ← 无出边（除 L0 内部）
      models/  models::tokens(TokenRecord)  models::FREE_MODEL_NAME
      error         (ProxyError/ProxyResult；依赖 reqwest，故不宣称零依赖)
      store         (StoreState + inspect_store_for)

L1  纯函数层 (pure)         ← 只依赖 L0
      translate/    (core / pipeline / stream / responses)
      billing       (WB 积分协议常量 + remaining_from_response)
      session       (三客户端 session id 方言映射)

L2  中立基础设施 (neutral)  ← 只依赖 std + 少量外部 crate；**禁止**依赖任何业务模块
      env           (.env + 进程环境读取)
      util::{text, time}   (字符串/时间)
      util::{redact, body} (axum/HTTP 相关)
      api_token     (constant-time 比较 + token 校验)

L3  持久化与域服务 (store)  ← 依赖 L0-L2
      stats  settings::paths  settings::logfile
      workbuddy_auth::{credential, parse, store, api_keys}
      client_accounts  pool_transfer  session_pool
      providers  settings::gui  settings::logbuf
      config  metrics  service  client_auth  api_token(handler 面)

L4  传输与编排 (transport)  ← 依赖 L0-L3
      router  proxy/  api  credits  console
      admin_state  admin  admin_rpc
      admin_routes  admin_auth
      workbuddy_auth::{refresh, points, checkin, oauth}
      growth  growth_runner  scheduler  scheduler_state
      claude_config  codex_config  dsh_config  launch_agent
```

**分层的判据只有一条：一条边只能从高编号指向低编号。** 违反者即环。

⚠️ **同一层内部的边不算「越层」，但仍可能构成环**——环 1（`admin ↔ admin_routes`）
就在 L4 内部，环 7 的 `settings ↔ providers` 也在 L3 内部。所以下表的「当前违规出边」
一列记的是**越层边**；同层成环的情况在「归位动作」列单独说明。

### 3.3 34 个模块逐个归位

| 层 | 模块 | 生产行 | 当前违规出边 | 归位动作 | 归位后消除的环边 |
|---|---|---:|---|---|---|
| **L0** | `models`（含 `anthropic`/`openai`/`responses`） | 894 | `models → stats`（`openai.rs:278`） | `TokenRecord` 下沉进来；加 `FREE_MODEL_NAME` | **环7 的 `models→stats`** |
| **L0** | `error` | 298 | 无 | 原样 | — |
| **L0** | `store`（新） | ~45 | — | 新建，搬 `StoreState`+`inspect_store_for` | **环6 的 `client_accounts→workbuddy_auth`** |
| **L1** | `translate`（含 4 子文件） | 2198 | `translate → util`（`pipeline.rs:315-316`） | 见下方「L1 的例外」 | — |
| **L1** | `billing`（新） | ~180 | — | 新建，搬积分协议 | **环4、环5 的 `workbuddy_auth→credits`** |
| **L1** | `session` | 527 | 无 | 原样（本来就是纯映射） | — |
| **L2** | `env`（新） | ~90 | — | 新建，搬 `env_value`/`env_lookup`/`non_empty_env`/`env_flag`/`env_u16`/`dotenv_values` | **环2/3/4 的 `X→config` 4 条边** |
| **L2** | `util`（拆 4 子文件） | 428 | `util → error`（`util.rs:286`） | 拆成 `text`/`redact`/`body`/`time`；`body` 保留 `→error` | — |
| **L2** | `api_token` | 119 | `api_token → config`（`:42`） | 改用 `env::value`；`constant_time_eq` 迁 `util::crypto` 属可选清理（见注） | **环3 的 `api_token→config`** |
| **L3** | `stats` | 1528 | `stats → settings`（`:9`） | `TokenRecord` 移出；`data_dir` 改依赖 `settings::paths` | **环7 的 `stats→settings`**（降级为 `→paths`） |
| **L3** | `settings::paths` | 86 | — | 从 `settings` 拆出 | — |
| **L3** | `settings::logfile` | 199 | — | 从 `settings` 拆出 | — |
| **L3** | `settings::logbuf` | 99 | — | 从 `settings` 拆出 | — |
| **L3** | `settings::gui` | 166 | `gui → providers`（策略方法） | 策略方法搬 `providers`；`gui` 只留结构+存取 | **环7 的 `settings→providers`** |
| **L3** | `client_accounts` | 538 | `→config`(539)、`→api_token`(434)、`→workbuddy_auth`(556-557) | 三条边分别由 `env`、`util/middleware`、`store` 替换 | **环2、环3、环6** |
| **L3** | `pool_transfer` | 1167 | 无违规（`→client_accounts`/`→workbuddy_auth`/`→settings` 均向下） | 原样 | — |
| **L3** | `session_pool` | 424 | `→workbuddy_auth`（向下，合法） | 原样 | — |
| **L3** | `workbuddy_auth::credential` | ~631 | — | 拆出 | — |
| **L3** | `workbuddy_auth::{parse,store,api_keys}` | ~659 | — | 拆出 | — |
| **L3** | `providers` | 477 | `providers → translate`（`:340/:343/:597/:600`） | `FREE_MODEL_NAME` 改从 `models` 取；**接收** `gui` 的策略方法 | **环7 的 `providers→translate`** |
| **L3** | `config` | 491 | `→client_accounts`(343)、`→workbuddy_auth`(257/285/286/288) | 删 `client_auth_enabled`（§1.3）；`workbuddy_auth` 那 4 条是**向下**读账号池，合法，保留 | **环2 的 `config→client_accounts`** |
| **L3** | `metrics` | 80 | 无 | 原样 | — |
| **L3** | `service` | 84 | 无 | 原样 | — |
| **L3** | `client_auth` | 228 | `→config`（`:191`） | 改 `env::value` | **环3 的 `client_auth→config`** |
| **L4** | `router` | 163 | 无 | 原样（README 说它是唯一注册点，实测确实只有 `bin`/`tauri`/测试注册） | — |
| **L4** | `proxy/`（6 文件） | **3454** | 无违规（14 条出边全向下） | 见 §4.1 的继续拆分 | — |
| **L4** | `api` | 505 | 无违规 | 原样；`stats_today_json`/`request_logs_json` 被 `admin`/`admin_routes`/`admin_rpc` 反向引用，但它们**是 JSON 形状函数**，应视为命名中立（§3.4） | — |
| **L4** | `credits` | 700 | `→config`（合法，向下）、`→session_pool`（合法） | 积分协议移出后只剩 handler | — |
| **L4** | `console` | 603 | 无违规 | 原样 | — |
| **L4** | `admin_state`（新） | ~30 | — | 新建，搬 `AdminState` | **环1 的 `admin→admin_routes`** |
| **L4** | `admin` | 837 | `→admin_routes`（3 处） | 参数降维（§1.1 A2）或 `AdminState` 迁出 | **环1** |
| **L4** | `admin_rpc` | 587 | 无违规 | 原样 | — |
| **L4** | `admin_routes` | 852 | 越层无（`→admin` 是同层边，构成环 1） | 原样（`AdminState` 迁出后它只是消费者） | **环1**的一半 |
| **L4** | `admin_auth` | 486 | `→api_token`（向下，合法） | 原样 | — |
| **L4** | `workbuddy_auth::{refresh,points,checkin}` | 899 | `points → billing`（向下） | 拆出 | — |
| **L4** | `workbuddy_auth::oauth` | 308 | — | **建议移入 `src-tauri`** | — |
| **L4** | `growth` | 949 | `→workbuddy_auth`（向下） | 原样 | — |
| **L4** | `growth_runner` | 625 | 无违规 | 原样 | — |
| **L4** | `scheduler` | 729 | 无违规 | 原样 | — |
| **L4** | `scheduler_state` | 267 | 无违规 | 原样 | — |
| **L4** | `claude_config` | 117 | 无违规 | 原样 | — |
| **L4** | `codex_config` | 568 | 无违规 | 原样 | — |
| **L4** | `dsh_config` | 956 | 无违规 | 原样 | — |
| **L4** | `launch_agent` | 268 | `→settings::logfile`（向下） | 原样 | — |

**注（`constant_time_eq` 的归位）。** 它是通用密码学工具，住在 `api_token`（L2）里，却让
`client_accounts`（L3）为了一个字节比较而依赖整个 token 校验模块
（`client_accounts.rs:434`）。**方向是错的，但不必急着改**：

- 单独迁移它（不动 `env`）：环数 **7 → 6**（只断掉边的末端，环 3 靠 `→config` 那两条边续命）。
- 在 P0（`env`）之后迁移它：环数 **5 → 5**，**零收益**——因为环 3 已经被 P0 杀掉了。

**所以它是「P0 之后的可选清理」，不是消环手段。** 若做：迁到 `util::crypto`（L2 → L3 向下合法），
改动 5 处（`api_token.rs:89`、`:120`、`:138-151` 测试、`client_accounts.rs:434`、`admin_auth.rs:314`）
+ `api_token.rs` 留 `pub(crate) use crate::util::crypto::constant_time_eq;` → **零调用点改动**。

**L1 的例外（`translate → util`）。** `pipeline.rs:315-316` 调 `util::local_utc_offset_secs()`
与 `util::unix_millis()` 来算「现在是不是夜间」，以决定 `free` 虚拟模型路由到 `hy3` 还是 `hy4`。
这是**唯一**让 L1 依赖 L2 的地方。

`pipeline.rs:299-301` 的注释说「`is_night_hour` 保持纯（不读时钟），由 `resolve_free_model`
把当前本地小时喂进来」——**这句话是准确的**，但它描述的 `resolve_free_model` 自己也没做这件事，
实际是 `is_night`（`:314`）在读时钟。**注解与实现已经分叉了。**

修法（不产生新环，但让 L1 名副其实）：把 `is_night(start, end)` 改成
`is_night(start, end, now_local_hour: i32)`，由 `proxy.rs`（L4）在调用链上把小时传下来。
`proxy.rs` 本来就在 `:1635` 附近用 `FREE_MODEL_NAME` 做路由判断，传一个 `i32` 是零负担。
改动：`pipeline.rs` 1 个签名 + 内部 1 处；`proxy.rs` 1-2 处调用点；`pipeline.rs` 现有 16 个
`translate/core` 测试与 58 个 `pipeline` 测试里涉及 `is_night` 的少数几个。
**风险中等，收益是「纯层真的纯」，不是消环**——所以排在 P6+，不进 3.3.0 的最小集。

### 3.4 归位后自然消失的环（汇总）

| 环 | 消失原因 | 依赖的拆分 |
|---|---|---|
| 1 `admin ↔ admin_routes` | `AdminState` 迁 `admin_state`（或参数降维） | `admin_state` |
| 2 `client_accounts ↔ config` | `env_value` → `env`；`config→client_accounts` 删除 | `env` + 删死字段 |
| 3 `api_token → config → client_accounts → api_token` | `env` 替换两条 `→config`；`constant_time_eq` → `util::crypto` | `env` + `util` + 删死字段 |
| 4 `config → workbuddy_auth → credits → config` | 积分协议 → `billing`，`workbuddy_auth→credits` 消失 | `billing` |
| 5 `credits → session_pool → workbuddy_auth → credits` | 同上（同一条边） | `billing` |
| 6 `client_accounts → workbuddy_auth → credits → config → client_accounts` | `StoreState` → `store` **且** 积分协议 → `billing` **且** `env`（三条边都要断） | `store` + `billing` + `env` |
| 7 `models → stats → settings → providers → translate → models` | `TokenRecord` → `models`；`FREE_MODEL_NAME` → `models`；`data_dir` → `settings::paths`；策略方法 → `providers` | `models` + `settings/paths` + `providers` |

**7 个环、涉及 12 条边、由 6 个新模块（`env`/`store`/`billing`/`admin_state`/`settings::paths`/
`util::crypto`）+ 2 次符号下沉（`TokenRecord`、`FREE_MODEL_NAME`）+ 1 次死字段删除全部消除。**

---

## 4. 风险：哪些拆分会让 704 个测试大幅改动

### 4.1 零风险 / 低风险 / 高风险分级（按实测站点数）

先看测试的分布（这是成本的真正决定因素）：

| 测试位置 | 测试数 | 说明 |
|---|---:|---|
| `src/**` 内联 `#[cfg(test)]` | **624** | 13828 行测试代码的主体；**几乎全部写 `use super::*`** |
| `tests/*.rs`（10 个文件） | **80** | 走 `proxy_rs::` 公共 API |
| 合计 | **704** | |

**关键事实：624 个内联测试（`src/**` 内）用 `use super::*`，这是「零改」的物理基础。**
只要被拆出去的东西仍然能通过 `super::*`（即父模块 `mod.rs` 的 re-export）解析到，
测试代码一个字都不用改。

| 拆分 | 受影响的测试 | 风险 | 说明 |
|---|---|---:|---|
| `env` 模块 | 0 | **零** | 4 个 `env_value` 调用点在生产代码里；`config.rs` 的 dotenv 测试**跟着函数搬文件**（`use super::*` 继续有效） |
| `store` 模块 | 0（若留 `pub use`） | **零** | `client_accounts.rs:846` 的 `StoreState::Corrupt(_)` 走 re-export |
| `billing` 模块 | 0 行改动（2 个测试**跟文件走**，断言不变） | **零** | `credits.rs` 4 个测试里 2 个属协议层（`:751` 测 `merge_segments`/`parse_workbuddy`、`:784` 测 `extract_generic_credits`）；另 2 个测 `fetch_credits` 留在 `credits.rs`；外部零引用 |
| `constant_time_eq` → `util::crypto` | 0（若留 `pub(crate) use`） | **零** | `api_token.rs:138-151` 的测试在新位置跑，断言不变 |
| `FREE_MODEL_NAME` → `models` | 0 | **零** | 全靠 `pipeline.rs` 一行 `pub use` |
| `TokenRecord` → `models` | 0 | **零** | `lib.rs:45` 与 `proxy`/`api` 的 re-export 保留 |
| 删 `Config::client_auth_enabled` | **4 行**（`tests/client_auth.rs:375/415/473/632`） | **低** | 且这 4 行是死配置（字段无人读），删掉不改变行为 |
| `admin` 参数降维 | 0 | **零** | `tests/admin_api.rs` 只构造 `AdminState` 给 `build_admin_router`，不调那 3 个函数 |
| `admin_state` 迁出 | 1 行（`tests/admin_api.rs:20`） | **低** | 若 `admin_routes.rs` 留 `pub use crate::admin_state::AdminState;` 则 0 行 |
| `settings` 拆 4 文件 | 0 | **零** | 359 行内联测试 / 17 个测试按组跟走，`use super::*` 有效；9 个集成测试文件走 `proxy_rs::settings::LogBuffer` |
| `util` 拆 4 文件 | 0 | **零** | 同上；`tests/router_redaction.rs`/`tests/body_buffer_limit.rs` 走 re-export |
| `workbuddy_auth` 拆 8 文件 | 0（**若 `mod.rs` 的 re-export 完整**） | **中** | 1040 行内联测试 / 51 个测试 + **`src-tauri` 64 个站点**。风险全在「re-export 漏了某个符号」 |
| `workbuddy_auth::oauth` 移入 `src-tauri` | `src-tauri` 内部 | **中高** | 首次把库代码移进 Tauri crate，需处理 `qrcode` 依赖的 feature 化 |
| `translate → util` 解除（§3.3 例外） | 16+58 个 `pipeline`/`core` 测试里涉及 `is_night` 的少数 | **中高** | 改的是**纯函数签名**，测试里的调用必须跟着改（这类测试**不是** `use super::*` 能救的，因为它们直接调 `is_night`） |
| `proxy.rs` 继续拆 | 2419 行 `tests.rs` 的**全部 54 个测试** | **最高** | 见 §4.2 |

### 4.2 最高风险：继续拆 `src/proxy.rs`

`src/proxy.rs` 生产 2250 行、`src/proxy/tests.rs` 2419 行 / 54 个测试。
这 54 个测试**大量直接调私有函数**（`super::forward_request`、`super::create_sse_stream`、
`super::json_request` 等，`super::` 命中已在 §1 前的扫描里确认有 15 个不同的 `super::X` 符号）。

**风险不在「能不能拆」，而在「拆完 `super::X` 是否还解析得到」。**
`proxy.rs:26-35` 已经给出了解法：`pub(crate) use <子模块>::*;` 全量 re-export。
但 §4.3 会说明「全量 glob re-export」在**继续拆分**时会遇到一个具体的坑。

**建议：`proxy.rs` 的继续拆分不进 3.3.0 的必做集。**
理由：它是唯一一个「拆分本身需要新写测试来验证行为未变」的地方（因为 `forward_request` 851 行
是核心路径），风险与 3.3.0「提高内聚、降低耦合」的收益不成正比。
3.3.0 先把 7 个环清零（零测试改动），3.4.0 再动 `proxy`。

若仍要在 3.3.0 拆 `proxy`，**最小且安全的一刀**是两个互相独立、可按任意顺序做的移动
（注意 `forward_request` 本身**不拆**，851 行的核心路径留到 3.4.0）：

```
src/proxy/
  aggregate.rs    # collect_stream_into_response(1912-1960) + Aggregate(1961-2087)
                  #   + non_streaming_response(2089-2197) + streaming_response(2199-2244)
                  ≈ 285 行
  models_list.rs  # list_models_handler(2246-2318) + list_models_via_config(2320-2392)
                  #   + translation_policy(2394-2405)
                  ≈ 150 行
```
注意这两块的调用关系（实测，别想当然）：`forward_request`（`1062-1912`）**确实调用了**
`collect_stream_into_response`、`non_streaming_response`、`streaming_response` 三个函数；
只有 `list_models_*` / `translation_policy` 与它无关。

这**不影响**拆分可行性，因为方向是 `proxy.rs → aggregate.rs`（父调子），正是 `proxy.rs` 现在
调 `sse::` / `failover::` 的同一模式：子文件顶部 `use super::*;` 拿到 `ApiFlavor`/`ProxyResult`/
`UpstreamTiming` 等，父文件用 `pub(crate) use aggregate::*;` 把符号收回命名空间。
**但「谁调谁」的口径必须写对**——`src/proxy/sse.rs:7` 的 `use super::*;` 与
`src/proxy.rs:31-35` 的 `pub(crate) use` 就是这套双向模式的现成先例。

移出后 `proxy.rs` 从 2250 降到 ~1815，且 `super::X`（`proxy/tests.rs` 的 54 个测试）继续有效。

### 4.3 「零改测试」的手法清单

按 `src/proxy.rs:26-35` 的先例，抽出成 6 条可复用规则：

#### 手法 1：路径别名 re-export（对付「消费方写死了 `crate::A::X`」）

```rust
// 旧宿主 A 里保留：
pub use crate::<新宿主>::X;
```
适用：`env_value`、`FREE_MODEL_NAME`、`TokenRecord`、`StoreState`、`AdminState`。
**效果：消费方一行不改，而旧宿主的出边消失**（re-export 产生的是 `B → 新宿主`，不是 `消费方 → B`）。

#### 手法 2：`pub use <子模块>::*;` glob（对付「测试写 `use super::*`」）

```rust
// src/settings/mod.rs
mod gui; mod logbuf; mod logfile; mod paths;
pub use gui::*;
pub use logbuf::*;
pub use logfile::*;
pub use paths::*;
```
适用：`settings`、`util`、`workbuddy_auth` 的拆分。
**效果：内联测试的 `use super::*` 原地生效，624 个内联测试零改动。**
⚠️ **注意**：`src/proxy.rs:31-35` 用的是 `pub(crate) use`，**不是** `pub use`。
原因写在 `proxy.rs:27-30`：「the test module is a *sibling* of these files (`proxy/tests.rs`),
so `super::X` resolves through `crate::proxy` and a private import would not be visible to it」。
对 `settings`/`util`/`workbuddy_auth` 而言，测试是**子模块**（`mod tests` 在被拆文件之内）而非
同级文件，`pub use` 就够；**但如果拆出的测试也变成同级文件（`settings/tests.rs`），必须
跟着改成 `pub(crate) use`**。这一条是最容易踩的坑。

#### 手法 3：模块目录 + `mod.rs` 保持 `pub mod <name>` 路径不变

`lib.rs` 的 34 行 `pub mod` **一行都不用改**——`pub mod settings;` 对
「`settings.rs` 单文件」和「`settings/mod.rs` 目录」是同一行。
**效果：`lib.rs` 与所有外部引用（`src-tauri`、`tests`）零改动。**

#### 手法 4：`pub(crate)` 收紧代替 `pub`

`constant_time_eq` 现在是 `pub(crate)`（`api_token.rs:120`）。迁到 `util::crypto` 后保持
`pub(crate)`，`util` 对外不新增公共 API——避免把内部工具变成版本承诺。

#### 手法 5：先加 re-export 再删旧定义（两步走，可独立回滚）

每一步 PR 只做一件事：
- PR-N：新模块 + `pub use` 兼容层 + 旧位置保留（**行为完全不变，测试必绿**）
- PR-N+1：删旧定义（此时只有 re-export 留在旧位置）

这样任何一步失败都能单点回滚，且每一步的 diff 都可机械审查。
**拆 `workbuddy_auth`（1040 行测试 + 64 个跨 crate 站点）时必须用这条。**

#### 手法 6：`#[doc(hidden)]` + `#[deprecated]` 标注过渡期

对 `config::env_value` 这类「以后不该有人再用的旧路径」：

```rust
#[doc(hidden)]
#[deprecated(note = "use crate::env::value")]
pub use crate::env::value as env_value;
```
**效果：现有调用点不报错（`deprecated` 是 warning），新代码被引导到新位置，
且 3.4.0 可以靠 `cargo build` 的 warning 清单确认迁移完成。**
⚠️ 本项目没有 `#![deny(warnings)]`（已确认 `lib.rs` 无 lint 属性），所以 warning 不会破坏构建。

---

## 5. 执行顺序（每步都可独立合入、独立验证）

| 步 | 内容 | 消环 | 生产改动 | 测试改动 | 风险 |
|---|---|---:|---|---|---:|
| **P0** | 新建 `env.rs`，搬 6 个环境变量函数 + `config` 留 `pub use` | **4 条边 / 2 个环** | ~90 行新 + 17 处内部改名 | 0（dotenv 测试跟搬） | 零 |
| **P1** | 新建 `store.rs`，搬 `StoreState`/`inspect_store_for` + 两处 re-export | 1 条边（环6 的一条腿） | ~45 行新 + 4 处 | 0 | 零 |
| **P2** | `constant_time_eq` → `util::crypto` + `pub(crate) use` | **0 个环**（见 §1 总览的更正） | ~15 行新 + 1 行 | 0 | 零 |
| **P3** | 新建 `billing.rs`，搬积分协议 + `credits` 留 `pub use` | **1 条边 / 2 个环** | ~180 行新 | 0（2 个测试跟搬，断言不变） | 零 |
| **P4** | 删 `Config::client_auth_enabled`（须在 P0 后） | 1 条边 / **环6 收口** | -3 行 | 4 行（死配置） | 低 |
| **P5** | 新建 `admin_state.rs`，搬 `AdminState` | **1 条边 / 1 个环** | ~30 行新 | 0（留 re-export） | 零 |
| — | **以上完成即 7 环中 6 个清零（剩环7），测试改动合计 4 行** | | | | |
| — | P2 可延后（不消环，仅改善依赖方向） | | | | |
| **P6** | `TokenRecord` → `models::tokens`；`FREE_MODEL_NAME` → `models` | **环7 断（2 条边）** | ~25 行 | 0 | 零 |
| **P7** | `settings` 拆 4 文件（paths/logbuf/logfile/gui） | — | 移动 ~550 行 | 0（glob re-export） | 低 |
| **P8** | `util` 拆 4 文件；`stats` 改依赖 `settings::paths` | 若 P6 未做则此处断 | 移动 ~428 行 | 0 | 低 |
| **P9** | `workbuddy_auth` 拆 8 文件（用 §4.3 手法 5 两步走） | — | 移动 ~2497 行 | 0（`mod.rs` re-export） | **中** |
| **P10** | `settings::gui` 的策略方法搬 `providers` | 若 P6/P8 未做则此处断 | ~65 行 + 12 调用点 | 0 | 中 |
| **P11** | `proxy.rs` 继续拆（`aggregate.rs` / `models_list.rs`） | — | ~410 行移动 | 0（glob re-export） | 中 |
| **P12**（3.4.0） | `workbuddy_auth::oauth` → `src-tauri` + `qrcode` feature 化 | — | ~308 行移动 | `src-tauri` 内部 | 中高 |
| **P13**（3.4.0） | `translate` 解除对 `util` 的时钟依赖（`is_night` 参数化） | — | ~10 行 | 少数 `pipeline` 测试 | 中高 |

**P0/P1/P3/P4/P5 是 3.3.0 的核心**：6 个环、8 条边、4 行测试改动、约 345 行新增（绝大部分是平移）。
**P2 不在其中**——它是可选清理（§3.3 的注），做完不减少任何环。

**这是本审计最重要的结论：7 个环里 6 个可以用「搬函数 + 删一个死字段」解决，
不需要「改设计」；剩下 1 个（环 7）才真的需要重新分配职责。**

---

## 6. 复核方法（任何人可复现）

```bash
# 0) 固定基线（本文件所有数字都来自这里）
git archive HEAD | tar -x -C /tmp/baseline && cd /tmp/baseline

# 1) 生产/测试行数（注意 proxy/tests.rs 是测试代码，由 proxy.rs 的 cfg(test) mod 引入）
#    规则：从文件内第一个同时满足「#[cfg(test)] 属性和其后 mod 声明/带花括号项」的位置切开；
#    src/proxy/tests.rs 整体记为测试。
wc -l src/*.rs src/*/*.rs | tail -1        # 42625

# 2) 依赖图（必须先去注释/字符串、去测试代码，否则误判）
#    本文件用的脚本逻辑：
#      a. 状态机剥离 //, 嵌套 /* */, "…", r#"…"#, char（区分 'a' 与生命周期 'a），保留换行
#      b. 花括号配平剔除 #[cfg(test)] 项；排除 src/proxy/tests.rs
#      c. 只匹配 crate::<lib.rs 里的 34 个 pub mod 名>
#    产物：136 条模块级边（417 个站点），7 个基本环，3 个 SCC

# 3) 手工抽查任意一条边（例：config -> client_accounts）
grep -n 'crate::client_accounts' src/config.rs     # 343 行

# 4) 测试总数
grep -rho '#\[test\]\|#\[tokio::test\]' src/ tests/ | wc -l   # 704

# 5) 环枚举校验：对每个 SCC 内的边集跑 Johnson 基本环算法
```

**为什么不能用粗糙的正则**（任务描述特别提醒过）：本仓库有多处会被误判的写法：

| 陷阱 | 例子 | 粗正则的错 |
|---|---|---|
| 注释里的路径 | `workbuddy_auth.rs:16` `//! Layer note: like 'credits.rs' …`；`client_accounts.rs:45` 同类 | 若写成 ``crate::credits``（文档链接）会被算成边 |
| 文档链接 `[`crate::x`]` | `admin_routes.rs:78` 注释里 `` `crate::console` `` | 同上 |
| 字符串里的内容 | 各处 URL、`.env` 变量名、错误文案 | 无影响（本例恰好没踩），但 `"crate::x"` 会被误算 |
| `use crate::x::{Y}` 换行 | 本仓库实测 0 处（已 grep 确认），但 `use crate::settings::{GuiSettings, DEFAULT_PORT};` 这种**单行多符号**形式必须按 `::` 前缀匹配而非按符号匹配 | 只匹配 `crate::settings::X` 会漏掉 `use crate::settings::{…}` 形式 |
| 测试代码里的依赖 | `proxy/tests.rs` 引用 19 次 `settings`、8 次 `session_pool` | 会把 `proxy→settings` 的扇入虚高，且把测试辅助代码当成架构依赖 |
| 子模块同名 | `src/proxy/sse.rs` 的 `super::*` 指向 `crate::proxy` | 顶层模块图里 `proxy/auth_headers` 等被折叠成 `proxy`，是**有意的**（对外契约单位是顶层模块） |

---

## 7. 一页结论

1. **环有 7 个（不是 5 个），归一为 3 个 SCC。** 任务清单漏了两个：`client_accounts → workbuddy_auth → credits → config → client_accounts`（4 环）和 **`models → stats → settings → providers → translate → models`（5 环）**——后者是唯一跨「纯核心/I/O 外壳」边界的环，性质最严重。
2. **`src/proxy.rs` 不是「22 行生产代码」，是 2250 行**（22 这个数字来自把 `#[cfg(test)] mod tests;` 声明当成测试体起点）。`proxy` 模块合计 3454 行生产代码，`forward_request` 单函数 851 行，**是全库最大的未拆分单体**。
3. **7 个环、12 条边，可以用 6 个新模块 + 2 次符号下沉 + 1 次死字段删除全部消除。**
   其中 **7 环里的 6 个由 P0/P1/P3/P4/P5 五步解决（测试改动合计 4 行）**，
   根因高度集中在三类：环境变量读取错位（4 条边 / 2 个环）、积分类协议错位（1 条边 / 2 个环）、
   一个共享的泛型文件健康检查（1 条边 / 环 6 的一条腿）。
   **第 7 个环只需再断任意一条边**（`models→stats` 或 `providers→translate` 最省）。
4. **`config → client_accounts` 和 `config.rs:343` 是一次没有消费者的计算**——`Config::client_auth_enabled` 全库只写不读，真正生效的开关在 `ClientAuthState`（两处调用者自己算，绕过 `Config`）。这是**纯减法**。
5. **三个 god module 的病灶不同，不能一刀切**：`settings` 是「4 件无关的事共用一个名字」（该拆）、`util` 是「名义扇入大但语义单一」（只该拆出 HTTP/axum 相关的两组）、`workbuddy_auth` 是「8 件事共用一个名字」（该拆：`oauth` 库内 0 消费方、`refresh`/`points` 各 2 个、`parse` 1 个——这四块共 979 行（200+398+308+73）本来就该独立）。
6. **零改测试的物理基础是 624 个内联测试都写 `use super::*`**，`mod.rs` 的 re-export 让它们原地生效。借鉴 `src/proxy.rs:26-35` 的先例，但注意那处必须用 `pub(crate) use`（测试是**同级文件**）而拆分目录通常 `pub use` 就够（测试是**子模块**）——这个区别是最容易踩的坑（§4.3 手法 2）。
7. **`proxy.rs` 的继续拆分与 `translate` 的纯化不应进 3.3.0**：前者需要新写测试来证明 851 行核心路径行为未变，后者要改纯函数签名（不是 re-export 能救的）。3.3.0 先拿「零测试改动的 6 个环」，3.4.0 再动这两处。
8. **一条反直觉的实测结论：`constant_time_eq` 迁出 `api_token` 不消任何环。** 它所在的三元环 `api_token → config → client_accounts → api_token` 会被 P0（`env` 模块）一次杀死两条 `→config` 边后自然消失。**不要为了「消环」而先做它**（§1 总览的更正段落）。
