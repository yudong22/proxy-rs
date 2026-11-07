# 对 `docs/plan-3.3.0.md` 的审阅与补遗

> 审阅时间：2026-11-07
> 审阅对象：`docs/plan-3.3.0.md`（基线 `76230dd`）+ `docs/audit-3.3.0/module-graph.md`
> 审阅时所在分支：`feat/session-browser`（基于 `76230dd`，已合入「会话页」功能）
>
> 本文结论用于修订 3.3.0 的执行方案，不改动原计划的基线快照（原计划的数字取自
> `76230dd`，作为可复现的历史记录保留）。

---

## 0. 执行顺序（本次确定）

1. **先修 BUG-1 / BUG-2**：它们是独立 bug，不依赖任何重构；且 BUG-1 是
   「安全开关假装生效」，不应在重构期间继续留在 `main` 上。用独立 hotfix 分支。
2. **再执行 3.3.0 计划**：以本文 §2 的补正为起点。

---

## 1. 核对结果：事实层几乎全对

按 `76230dd` 基线逐条复算，原计划的严谨度很高：

| 断言 | 核对 | 结果 |
|---|---|---|
| Tauri 54 / RPC 48 / REST 31 | 基线精确匹配 | ✅ |
| `workbuddy_auth` 2515 / `settings` 557 / `util` 428 / `stats` 1528 生产行 | 与 `module-graph.md` §0.2 口径一致 | ✅ |
| 7 个环的根因（`admin.rs` 收 `Arc<AdminState>`、`config↔client_accounts`、`api_token` 环） | 逐条定位属实 | ✅ |
| BUG-2 桌面端 `client_account_reissue` 命令未注册 | 当前分支仍是 0 处，真 bug | ✅ |
| BUG-1 服务器端不热应用入站鉴权 | `admin.rs:218` 只写文件、`main.rs:591` 才 `set_enabled`，属实 | ✅ |
| `#[cfg(test)]` 拆分时 `pub(crate) use` 的坑 | `proxy.rs:28-35` 有实证 | ✅ |

**当前分支已把这些数字推到 57 / 51 / 34、测试 727**（原计划写 704）。
原计划写于基线，本身没错，但作为执行依据必须注意 §2.1。

---

## 2. 需要修正 / 补正的问题

### 2.1（最重要）计划已被 `feat/session-browser` 半落地，S1 的起点变了

原计划 §7 说「`claude_sessions`（并发会话正在开发）与本次重构冲突，需协调」。
现在会话页**已经提交**，而且它自己就是 S1 的一个成功样本：

- `src/claude_sessions.rs`：纯逻辑 / 文件 IO 放进 library crate，同步、不碰 axum/Tauri；
- `src/admin.rs`：`claude_sessions_list/detail` 是**唯一实现**；
- `src/admin_rpc.rs`（3 个 arm）、`src/admin_routes.rs`（3 条 REST）、
  `src-tauri/src/main.rs`（3 个 `#[tauri::command]`）全部只做「取参数 + 转调 `admin::`」。

这正好是原计划 §2.3「`admin_rpc` 退化为薄 dispatch / 桌面端也只留取参数」的
**目标形态**，且已跑通（Tauri / server 两端共用同一份 `ui/`）。

> **修订**：S1 不要从零另建 `src/admin_ops/`，否则会出现 `admin.rs` 与
> `admin_ops/` 两个共享层。应把「会话页三层转调」提炼为模板，明确写
> 「以后新功能一律照此办理」，再把 §2.1 的 4 组重复函数收编进 `admin.rs`
> 这同一层（必要时 `admin.rs` 内分子模块，而不是新开平级 crate）。

### 2.2 决策 A 的事实有误 —— REST 面有外部契约

原计划 §2.4 / 决策 A 说「31 条 REST 路由**实测无脚本消费者**」，据此把
「砍掉 REST 面」列为选项 2。但：

- `deploy/linux/api.wuba.site.conf:143` 的 nginx 把 `/admin/` 整段反代，
  REST 与 RPC 共用这个入口；
- `docs/admin-api.md:13` 明确对外承诺 `/admin/api/<resource>` 给 curl / 自动化用；
- `tests/admin_api.rs` 里 8 条 REST 路由有测试（原计划 §2.4 亦自认「有测试」）。

「无脚本消费者」≠「无外部契约」——这是**已发布的 API**。砍掉是破坏性变更。

> **修订**：决策 A 直接收敛为「保留」。选项 2 降级为「需重写迁移说明 +
> 破坏性版本 + 反代配置同步」，不再与选项 1 并列为等价推荐。
> 精力放在「REST 与 RPC 共用同一份 handler 实现」上。

### 2.3 `Config::client_auth_enabled` 的死代码要指到具体那一刀

原计划 §3 说它「全库只写不读」，但 `src/config.rs:343` 的 `from_settings` 里
`self.client_auth_enabled = enabled_from(...)` 是**自赋值改写**，
`src/bin/proxy-rs-server.rs:160` 又读了一次 `settings.client_auth_enabled`
（注意是 `settings` 不是 `config`）。

真正的死代码是 **`Config` 结构体里那一列折算后的值**（构造后没人从
`cfg.client_auth_enabled` 读），而断掉 `config → client_accounts` 这条边的
是 `from_settings` 里那行 `enabled_from(...)` 折算，**不是**删字段本身。

> **修订**：S7 必须写清「删字段」与「删折算行」两步分别做什么；
> 只删字段而留着折算行，环不会消失。

### 2.4 S4/S5/S6 的「零风险」缺自动守卫

三个新模块（`env.rs` / `store.rs` / `billing.rs`）都是纯搬迁，确实零逻辑改动，
但**环是否真的断了没有人自动验证**。原计划 §5.3 验收第 4 条说「重跑
module-graph.md §6 的脚本」——这是**手工**步骤。

> **修订**：既然 §7 已计划加「结构守卫测试」防两端漂移，就把「环数 = 0」
> 也做成**同一个 CI 里的自动断言**（一个解析 `use crate::` 的测试即可），
> 而不是靠人记得跑脚本。否则 3.3.0 之后新加一个 `use` 就悄悄长回环。

---

## 3. 补充内容（对应「提高内聚、便于扩展更多功能」）

### 3.1 补「新功能落位规约」

原计划全篇讲「怎么拆旧的」，没回答「下次加功能该放哪」——而内聚的
目的正是这个。新增一节，直接复用会话页的现成结论：

| 层 | 放哪里 | 约束 |
|---|---|---|
| 纯逻辑 / 文件 IO | `src/<domain>.rs`（library crate） | 同步，不 import axum/Tauri；阻塞 IO 由调用方 `spawn_blocking` |
| 共享实现 | `src/admin.rs` 一个 `pub async fn` | 唯一业务实现 |
| 传输面 ×3 | `admin_rpc` arm / `admin_routes` route / `#[tauri::command]` | **只做参数解析 + 转调，禁止写业务分支** |
| 前端 | `ui/src/views/<name>/` | 新 JS/CSS 必须登记 `src/console.rs` 的 `ASSETS`，否则服务端 `/admin/` 404 |
| 标签页 | `ui/src/views/tabs.js` + hash | overview 保持第一（它是默认激活页） |

> `console.rs` 的 `ASSETS` 白名单是本仓库最容易漏的第二处「加功能要记得改两处」，
> 会话页踩过；应纳入结构守卫测试。

### 3.2 补「拆分后的模块边界契约」

S9/S10 拆完 `settings` / `workbuddy_auth` 后，用 `pub use` 兼容层做到
零测试改动是对的，但**兼容层本身会掩盖新的错误依赖**（谁都可以继续
`use settings::*`）。

> 明确：兼容层只保留一个版本周期；同时给出「拆完后哪些 `use` 路径被废弃」
> 的清单，避免 `pub use *` 变成永久隐式耦合。

### 3.3 补 `ui/` 侧的内聚问题

原计划只审视 `src/`（Rust），但 P1 的另一半是「同一份 UI 跑在两种部署」：

- `ui/src/main.js` 是全局装配 + `switchTab` 混合，hash 路由已加（本次）；
- `console.rs` 的 `ASSETS` 白名单是第二处「加功能要记得改两处」。

> 把这两条也纳入「结构守卫测试」的断言范围。

### 3.4 S10 缺验收口径

「零测试改动」是好事，但也意味着**测试没有跟着模块边界走**——拆分后
每个子模块应至少有 1 个能单独跑的测试，否则「内聚」只是目录好看。

> 给 S10 加一条：拆完为每个子模块补一个最小单测（可以只是「公共 API
> 存在且可调」），确保边界是真的。

### 3.5 优先级：BUG-1 / BUG-2 提前为 S0

BUG-1（安全开关假装生效）与 BUG-2（重新签发必然失败）都是**不依赖任何
重构的纯 bug**，且 BUG-1 严重级别更高。S1 一旦动 `admin.rs` 会改到同一片
代码，先修更干净。

> 修订步骤表：新增 **S0（hotfix，独立分支，先合）** = 原 S2 + S3；
> 原 S1 顺延。见 `docs/plan-3.3.0.md` §5.1 的步骤编号保持不变，仅在
> 执行顺序上把 S0 插到最前。

---

## 4. 修订后的执行清单

| 序 | 内容 | 来源 | 验证 |
|---|---|---|---|
| **S0** | 修 BUG-1（保存后热应用入站鉴权，两端共用）+ 修 BUG-2（补 `client_account_reissue`），各带**修复前会失败**的回归测试 | 本审阅 §3.5 | `cargo test`，两测试先红后绿 |
| S1 | 以「会话页三层转调」为模板收编 §2.1 的 4 组重复函数（收进 `admin.rs` 同一层，不另建平级共享层） | 原 S1 + 本审阅 §2.1 | 4 组函数在仓库只剩一份定义 |
| S2 | 建结构守卫测试：两端共享函数名一致 + **环数 = 0** + `console.rs ASSETS` 覆盖 `ui/` 全部模块 | 原 §7 + 本审阅 §2.4 / §3.3 | CI 自动断言，非手工 |
| S3 | 决策 A 定为「保留 REST」，REST / RPC 共用 handler | 本审阅 §2.2 | — |
| S4–S8 | `env.rs` / `store.rs` / `billing.rs` / 删 `Config` 折算行与死字段 / `admin_state.rs` | 原 S4–S8 + 本审阅 §2.3 | 环 7→0（S2 守卫自动验） |
| S9 | `settings` 拆 4 子文件 + 兼容层保留一个版本周期 + 废弃路径清单 | 原 S9 + 本审阅 §3.2 | `git diff -M --stat` 全是 rename |
| S10 | `workbuddy_auth` 拆 8 子文件，每子模块补 1 个最小单测 | 原 S10 + 本审阅 §3.4 | 零逻辑改动 + 边界可测 |
| 文档 | 补「新功能落位规约」一节 | 本审阅 §3.1 | — |

验收标准沿用原计划 §5.3 六条，另加：

7. 结构守卫测试在 CI 中运行（环数、命令名对照、`ASSETS` 覆盖全部自动断言）
8. 会话页的「三层转调」被写成新功能模板，并有守卫测试防止新命令绕过共享层

---

## 5. S0 施工细节（已核实，2026-11-07）

### 5.1 BUG-1：只抽「热应用函数」，两端各调一次

决策（用户拍板）：**不**把整个 `save_settings` 上移成共享层，只抽一个
"保存后热应用入站鉴权"的函数，改动小、零风险、且不新增第三份业务逻辑。

已核实的约束（Explore 复核）：

| 事实 | 位置 |
|---|---|
| `admin::settings_json()` 返回**原始** `client_auth_enabled`；`ProcessInfo::status_json()` 才是 `enabled_from(...)` | `src/admin.rs:127` / `:95` |
| `SettingsUpdate.client_auth_enabled: Option<bool>`，`None` = 保持不变 | `src/admin.rs:168,218` |
| `admin::save_settings` 是 sync、无 state、返回 `settings_json()` | `src/admin.rs:175` |
| `client_accounts::enabled_from(flag)` 读 `PROXY_CLIENT_AUTH`（`config::env_value`，env 可双向覆盖） | `src/client_accounts.rs:538` |
| `ClientAuthState::set_enabled` sync / `reload_from_store` async（只重载 accounts，不动 enabled 标志，故两者都要调） | `src/client_accounts.rs:482,486,492` |
| 服务端 `AdminState.client_auth` 与数据面守卫是**同一个 Arc** | `src/bin/proxy-rs-server.rs:221` |
| `admin::save_settings` 只有 2 个调用点（RPC arm + REST handler），各自带一段重复日志 | `src/admin_rpc.rs:97` / `src/admin_routes.rs:564` |
| 桌面参考实现用 `body.client_auth_enabled`（非 Option）折算后 `set_enabled` + `reload` | `src-tauri/src/main.rs:591-593` |

**新增共享函数**（放 `src/admin.rs`，供三种调用方复用）：

```rust
/// 保存设置后，把入站鉴权开关热应用到运行中的状态。
/// `stored_flag` 是**保存后**文件中的原始值；env 覆盖在此统一折算，
/// 避免两端各写一遍 `enabled_from`。
pub async fn hot_apply_client_auth(
    state: &std::sync::Arc<crate::client_accounts::ClientAuthState>,
    stored_flag: bool,
) {
    let enabled = crate::client_accounts::enabled_from(stored_flag);
    state.set_enabled(enabled);
    state.reload_from_store().await;
}
```

调用点：
- `admin_rpc` `save_settings` arm、`admin_routes::settings_save`：保存后从
  `GuiSettings::load().client_auth_enabled` 取 `stored_flag`（覆盖 `None` 场景），调 `hot_apply_client_auth(&state.client_auth, flag).await`。
- 桌面 `save_settings`：把 `:591-593` 三行替换为
  `proxy_rs::admin::hot_apply_client_auth(&ctx.client_auth, body.client_auth_enabled).await;`
  （`enabled_from` 由共享函数统一做，桌面不再自己折算）。

> 注意：`PROXY_CLIENT_AUTH` env 覆盖时，`get_settings` 与 `get_status` 仍可能
> 读到不同值（§5.1 第一行）。这属于既有口径差异，**S0 不扩大改动面**，
> 记入 S1 的契约收编清单。

**回归测试**（`tests/admin_api.rs`，修复前必失败）：
构造带 `Arc<ClientAuthState>` 的 `AdminState`（保留 clone 以便断言），
`isolate(name, Some("tok"))` + `login_cookie`，POST `client_auth_enabled: true`
到 RPC `save_settings`（body 形状 `{ "body": {...} }`）与 REST `settings_save`
（直接 `SettingsUpdate` JSON），断言 `state.client_auth.is_enabled() == true`。
需保证测试进程 `PROXY_CLIENT_AUTH` 未设置（否则 `enabled_from` 被覆盖）。

### 5.2 BUG-2：桌面补 `client_account_reissue`

已核实的契约：UI reissue handler 读 **`res.api_key` + `res.account.label`（nested）**
（`ui/src/views/settings/client-accounts.js:365-370`）——**不是** desktop create
的 flat 形状。`admin::reissue_client_account_key(id)` 正好返回
`{account, api_key, header}`（nested），所以桌面命令**直接调它**即可自动对齐，
不要照抄 `client_account_create` 的 JSON。

```rust
#[tauri::command]
async fn client_account_reissue(
    ctx: State<'_, Arc<AppContext>>,
    id: String,
) -> Result<Value, String> {
    let out = tokio::task::spawn_blocking(move || {
        proxy_rs::admin::reissue_client_account_key(&id)
    })
    .await
    .map_err(|e| e.to_string())?
    .map_err(|e| e.to_string())?;
    ctx.client_auth.reload_from_store().await;
    Ok(out)
}
```

注册进 `generate_handler!`（与 `client_account_create/toggle/delete` 并列，
`src-tauri/src/main.rs:2800-2802`）。桌面命令可直接调 `proxy_rs::admin::*`
（会话页三命令已是先例，`src-tauri/Cargo.toml` 依赖 `proxy-rs`）。

**回归测试**：新增「结构守卫测试」的雏形——source-parse `generate_handler![...]`
的命令名集合，断言包含 UI 里 `invoke('client_account_*')` 用到的全部四个名字。
此测试同时是 S2 守卫的起点。

### 5.3 一句话结论

**方向正确、证据扎实，主要风险不在分析而在「时点」**：原计划基于 `76230dd`，
而 `feat/session-browser` 已经把 S1 的目标形态落地了一半、并顺带改了
命令 / 路由 / 测试数。以「会话页三层转调」为现成模板，修正决策 A 的
「无消费者」事实、把环数断言做成 CI 自动守卫、补「新功能落位规约」，
并把 BUG-1 / BUG-2 提前成独立 hotfix（S0）—— 然后按 §4 上表执行。
