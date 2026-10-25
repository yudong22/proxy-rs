# proxy-rs v2.2.0 功能规划：国内版积分增长自动化

> 目标版本：**2.2.0**（当前 2.1.10）
> 特性：把 WorkBuddy 国内版的「成长任务中心」自动化——自动接取、推进、领取积分，
> 让账号池里的积分自己长起来，而不是只能靠每日签到那一点。

---

## 1. 目标与非目标

### 1.1 目标

| # | 目标 | 验收方式 |
|---|---|---|
| G1 | 一键/定时跑完**可自动化**的成长任务，把能领的积分领到手 | 单账号一次运行后 `energy.balance` 增长，日志列出「领取 N 个任务 +M 分」 |
| G2 | 清楚区分「可自动化」「需真人」「不可伪造」三类任务，不假装成功 | 面板对不可自动化任务显示真实原因与深链，绝不虚报进度 |
| G3 | 失败可诊断：接取被拒、上报无效、领奖 400 都有原始 code/msg | 日志逐任务记录，不出现「跑了但什么都没发生」 |
| G4 | 不触发上游风控：账号间错峰、单账号节流、受限即停 | 命中 403/11140 的账号持久化标记并跳过，不连坐其他账号 |
| G5 | 只对国内版生效；国际版与企业号安全跳过 | 国际版账号按钮隐藏；日志明确「不适用」 |

### 1.2 非目标（本版不做）

- **不做国际版**（用户已决定移出 2.2.0）。国际版实测结论见 §8，留档备查。
- **不伪造不可伪造任务**。`wb_wechat_oa_subscribe_task`（关注公众号满 24h）、
  `Expert_Philanthropy`（真实捐款）永远跳过。
- **不绕过桌面端专属任务**。`RichMeow_Chat` / `Library_read` / `Buddy_App` /
  `Buddy_App_QQ` 只认桌面客户端真实行为信号，伪造上报会被忽略；本版**诚实跳过并给深链**。
- **不做跨账号积分转移**。积分是账号级，不可搬。
- **不改** 已有的每日签到逻辑（`batch_claim_daily_checkin`），两者并存。

---

## 2. 实测结论（2026-10-25，用真实账号 `libai` 验证）

> 这一节是本规划的地基。所有数字都是当场跑出来的，不是推断。

### 2.1 完整链路已跑通

```
接取 chat_5        POST /v2/activity/growth/tasks/accept   -> {"status":"accepted"}
上报 5 次对话事件   POST /v2/report                         -> code=0
进度从 0/5 → 5/5   GET  /v2/activity/growth/tasks           -> status=completed
领奖              POST /activity/growth/tasks/chat_5/claim -> {"credit":100,"energy":5}
余额                GET  /v2/activity/growth/energy          -> balance 8 → 13
```

**真实获得 100 积分**。这是本版可行性的直接证据。

### 2.2 该账号当前任务清单（20 个）

| 状态 | 数量 | 说明 |
|---|---|---|
| 已领取 | 7 | `create_canvas` / `playbook_prompt` / `RichMeow_Chat` / `Library_read` / `Expert_lighthouse` / `Hp_Appearance` / `Buddy_App` / `first_buddy` |
| 待做 | **11** | 总奖励 **900 积分** |
| 待做但不可自动化 | 1 | `wb_wechat_oa_subscribe_task`（需关注公众号满 24h） |

剩余可拿的 11 个任务：

| 任务 | 奖励 | 可自动化 |
|---|---|---|
| `chat_5` 发起 5 次对话 | 100 | ✅ 已实测跑通 |
| `expert_5` 召唤 5 次专家 | 100 | ✅ 事件上报 |
| `template_5` 使用 5 个模板 | 100 | ✅ 事件上报 |
| `automation_1` 设置自动化任务 | 100 | ✅ 事件上报 |
| `skill_1` 尝鲜热门技能 | 100 | ✅ 事件上报 |
| `Expert_team_use_3` 召唤 3 次专家团 | 100 | ✅ 需不同 team id |
| `WorkBuddy_Valorant` 体验拼豆专家 | 100 | ✅ 需特定 expert id |
| `expert_use_maidehao` 等 4 个专家体验 | 各 50 | ✅ 需各自 expert id |
| `wb_wechat_oa_subscribe_task` 关注公众号 | 100 | ❌ 需真实关注 24h |

### 2.3 两个端点都可用的观察

`copilot.tencent.com` 与 `www.codebuddy.cn` 对成长接口**都返回 200 且数据一致**
（energy=8、streak=17）。参考实现默认打 chat 侧（`copilot.tencent.com`），
本版沿用，但保留可切换。

---

## 3. 现状摘要

| 领域 | 现状 | 影响 |
|---|---|---|
| 现有积分能力 | 仅有每日签到 `batch_claim_daily_checkin`（`src/workbuddy_auth.rs:2596`），单一动作 | 本版新增成长任务模块 |
| 积分展示 | `credits.rs` 查 `/v2/billing/meter/get-user-resource`，由 scheduler 定期刷新 | 复用现有 `points` 字段，无需新存储 |
| 调度器 | `src/scheduler.rs` 已有两个循环：每日打卡（60s tick）、登录态巡检（300s tick） | **新增第三个循环**，不改前两个 |
| 账号池 | `CredentialPool` + `WorkBuddyCredential`，已有 `cooldown_until_ms` / `last_error` 语义 | 复用现有节流字段 |
| 命令层 | 47 个 `tauri::command`，含 `wb_checkin_all` / `wb_checkin_single` | 新增 3 个命令 |
| 日志 | `LogBuffer::push` 落盘 + GUI 实时视图 | 逐任务记录 |
| 国际版 | 实测成长接口可用，但任务只认真实桌面行为；无签到 | 本版跳过，见 §8 |

**结论**：不需要新存储层、不需要改选号逻辑。需要的是一个**成长任务客户端 + 一个新调度循环 + 3 个命令 + 面板区**。

---

## 4. 核心设计

### 4.1 任务分类（三档，必须显式建模）

这是本版最重要的一处设计：**不能把所有任务一视同仁地"尝试一遍"**，否则会得到
大量 400「task not completed」和虚报成功。

```rust
/// 一个成长任务的可自动化程度。
pub enum Automation {
    /// 事件上报即可推进（对话、专家、模板、技能、自动化）
    Auto,
    /// 只认桌面客户端真实行为，伪造上报无效 —— 诚实跳过，只给深链
    DesktopOnly { hint: &'static str },
    /// 物理上不可能自动化（关注公众号满 24h、真实捐款）
    Unforgeable { reason: &'static str },
}
```

**分类表**（静态常量，源自参考实现 2026-09 实测校准 + 本次实测）：

| code | kind | 事件 eventCode | target | 奖励 |
|---|---|---|---|---|
| `chat_5` | chat | `chat_request_send` | 5 | 100 |
| `expert_5` | expert | `expert_actual_use` | 5 | 100 |
| `Expert_team_use_3` | team | `expert_actual_use` (type=team) | 3 | 100 |
| `template_5` | template | `agent_task_created_with_template` | 5 | 100 |
| `skill_1` | skill | `skill_info` | 1 | 100 |
| `automation_1` | automation | `automated_task_create_suc` | 1 | 100 |
| `create_canvas` | canvas | `wbx_design_canvas_task_create` | 1 | 300 |
| `playbook_prompt` | playbook | `playbook_prompt_send` | 1 | 100 |
| `Hp_Appearance` | skin | `appearance_skin_apply` | 1 | 100 |
| `Model_chat_GLM5.2` | glmchat | `chat_request_send` (glm) | 1 | 100 |
| `black_cat` | cat | `chat_request_send` (night) | 3 | 100 |
| `WorkBuddy_Valorant` | expert | `expert_actual_use`（固定 expert id） | 1 | 100 |
| `expert_use_*`（4 个） | expert | `expert_actual_use`（各自 id） | 1 | 50 |
| `RichMeow_Chat` | — | — | — | 100 → `DesktopOnly` |
| `Library_read` | — | — | — | 100 → `DesktopOnly` |
| `Buddy_App` | — | �� | — | 100 → `DesktopOnly` |
| `Buddy_App_QQ` | — | — | — | 100 → `DesktopOnly` |
| `wb_wechat_oa_subscribe_task` | — | — | — | 100 → `Unforgeable` |
| `Expert_Philanthropy` | — | — | — | 0 → `Unforgeable` |

**关键约束（源自参考实现实测校准）**：

1. **专家/团队事件按 `(eventCode, id)` 去重**，同一 id 重复上报不推进。必须准备
   **id 池**（参考实现有 18 个已验证 expert id、3 个 team id）。
2. **`first_buddy` 是 auto 类且是其他任务的前置**，必须先点亮并领奖，否则
   `single` 任务接取会报 `prerequisite not met: first_buddy`。
3. **`black_cat` 只在 23:00–08:00 上报才计数**，且每天 1 次、累计 3 天——需要跨天状态。
4. **`auto` 类任务不接受接取**（`accept` 会失败），直接上报即可。
5. **事件必须带 `userId`**，否则上游 200 但静默丢弃。

### 4.2 模块划分

新增 `src/growth.rs`（Layer 3，有 I/O，与 `credits.rs` 同级）：

```rust
pub struct GrowthClient<'a> { client: &'a reqwest::Client, endpoint: &'a str }

impl GrowthClient<'_> {
    pub async fn tasks(&self, cred: &WorkBuddyCredential) -> Result<Vec<GrowthTask>>;
    pub async fn accept(&self, cred, codes: &[String]) -> Result<AcceptReport>;
    pub async fn report(&self, cred, events: &[Value]) -> Result<bool>;
    pub async fn claim(&self, cred, code: &str) -> Result<ClaimReport>;
    pub async fn energy(&self, cred) -> Result<f64>;
}

/// 单账号一次增长的完整结果。
pub struct GrowthRun {
    pub credential_id: String,
    pub label: String,
    pub accepted: usize,
    pub completed: usize,
    pub claimed: usize,
    pub credits_earned: f64,
    pub skipped: Vec<SkippedTask>,   // 含原因与深链
    pub errors: Vec<String>,
}
```

### 4.3 单账号执行流程

```
1. 预检：realm 必须 Cn；非企业号；未被 restricted
2. 取任务清单
3. auto 类（如 first_buddy）先推进 + 领奖        ← 前置依赖
4. 批量接取 Auto 且 status=not_accepted
5. 复核接取结果（上游可能瞬时拒绝整批，需重取一次补接）
6. 对每个未完成任务：
   a. 计算结果需要的上报次数 = target - current
   b. 生成对应事件（每个事件用不同 id，避免去重）
   c. 上报 → 等待 → 复核进度
   d. 达到 target 则 claim
7. 汇总；更新账号 points
```

**节流（必须）**：
- 账号之间随机错峰 2–5 秒（防并发风控，参考实现 issue #394）
- 单账号内每次上报间隔 ≥1 秒
- 单账号总预算上限（默认 120 秒），超时优雅中止并保留已完成部分
- 命中 403 / code `11140` → 持久化受限标记，本轮立即停该账号

### 4.4 调度

`scheduler.rs` 新增第三个循环 `run_growth_tasks`：

- tick 间隔：**1 小时**（成长任务不像签到那样有时点，但要避开风控节奏）
- **默认关闭**（用户已确认）：由设置页开关 `growth_enabled` 控制，开启后才调上游
- 每个账号每天最多跑一次（`last_growth_run_date`）
- **夜间窗口跑 `black_cat`**（用户已确认）：该任务只在 23:00–08:00 上报才计数，
  因此调度器需要在该窗口内至少醒来一次。实现为一个**独立的夜间触发**，
  而不是把 1 小时 tick 无脑跑通宵：
  - 22:55 之后第一次 tick 时检查「今天是否已跑过夜间批」，
  - 未跑过则只跑 `black_cat` 这一个任务（不做全套）、每账号一次、目标 1 次进度
  - 记录 `last_night_growth_date` 防重复；跨零点后归入新的一天
  - 常态白天 tick 遇到 `black_cat` 一律跳过（避免打无效上报）
  - **注意 `black_cat` 的 target 是跨天的**：源码注释为「每天 1 次、累计 3 天」，
    而上游的 `progress.current` 是累计值。因此不要按「一次跑满 3 次」去补——
    同一天连发 3 次只会计 1 次（上游按天去重）。一天一次、连跑 3 天即可，
    进度会自己从 1/3 → 2/3 → 3/3。若上游把 target 报成 1/1 则说明该任务已改口径，
    按上报的 target 执行，不做假设。

### 4.5 存储扩展（`PoolPreferences`）

```rust
pub struct PoolPreferences {
    // … 现有字段 …
    /// 是否自动跑成长任务
    #[serde(default)]
    pub growth_enabled: bool,
    /// 上次自动运行日期（本地日 YYYY-MM-DD），防一天多次
    #[serde(default)]
    pub last_growth_run_date: Option<String>,
}
```

账号级复用现有 `cooldown_until_ms`（受限语义），新增：

```rust
#[serde(default)]
pub last_growth_run_date: Option<String>,
```

### 4.6 命令层

| 命令 | 说明 |
|---|---|
| `wb_growth_run` | 对全部可用国内版账号跑一次（手动按钮） |
| `wb_growth_run_single(id)` | 单账号跑一次（行内按钮） |
| `wb_growth_preview(id)` | 只读：返回任务清单 + 分类 + 预估可得积分（**不产生任何写操作**） |

`wb_growth_preview` 很重要：让用户在动手前看清"哪些能自动、哪些要自己点"。

### 4.7 GUI

`ui/src/views/settings/credential-pool.js`：
- 工具栏新增「增长积分」按钮（跑全池）
- 账号行新增「增长」按钮，与「打卡」「刷新积分」并列
- 新增**成长任务面板**（模态）：列出每个任务的状态、奖励、可自动化程度；
  不可自动化的显示原因 + 深链（`workbuddy://` 或网页链接）
- 国际版账号行**不显示**该按钮
- 新增设置项：`growth_enabled` 开关 + 执行时间窗说明

---

## 5. 分阶段实施（3 个 PR）

| 阶段 | 内容 | 验收 |
|---|---|---|
| **P1 客户端** | `src/growth.rs`：任务清单/接取/上报/领奖/energy + 分类表 + 单测（用 mock 响应） | 单测覆盖「进度推进」「去重不推进」「claim 400」等分支 |
| **P2 编排与调度** | 单账号流程、节流、受限标记、新调度循环、3 个命令 | **手工**：真实账号跑一次，`energy` 增长，日志逐任务可读 |
| **P3 面板** | 成长任务模态、行内按钮、设置开关、国际版隐藏 | `ui-smoke` 断言；手工点一遍 |

**P1 可独立合入**（纯客户端，无副作用，默认不调用）。

---

## 6. 测试计划

| 层次 | 用例 |
|---|---|
| 单元 | 事件生成：每个 kind 产出正确的 `eventCode` 与必填字段（`userId` 缺失会被上游静默丢弃） |
| 单元 | **去重**：同一 expert id 连发 2 次只应计入 1 次（断言生成器保证 id 不同） |
| 单元 | 分类：`DesktopOnly` / `Unforgeable` 的任务**绝不出现在可执行列表** |
| 单元 | `black_cat` 在非夜间窗口被跳过 |
| 单元 | `prerequisite not met` 时先跑 `first_buddy` |
| 单元 | claim 返回 400 时不虚报成功 |
| 单元 | 节流：账号间间隔 ≥2s（用假时钟断言） |
| 集成 | mock upstream 全流程：accept → report → progress → claim，断言最终 credits |
| 手工 | 真实账号跑一次，**对照 `energy` 前后差值** |
| 手工 | 受限账号（403/11140）被跳过且标记持久化 |

**真实账号验证是本期硬要求**：单测证明代码正确，但"上游是否真的给积分"只能实测。

---

## 7. 风险与注意事项

| 风险 | 影响 | 缓解 |
|---|---|---|
| **上游反自动化升级** | 上报返回 200 但进度不动（国际版已实测如此） | 每次上报后**复核进度**；若连续无变化则中止并记录「上游可能已加校验」，不虚报成功 |
| **账号级风控连坐** | 同设备多账号一起被限 | 受限即持久化标记并跳过；账号间错峰；默认关闭自动调度 |
| **重复领取/刷分** | 可能触发封号 | 严格按 `target` 计算上报次数，不多发；已 claimed 的任务不再碰 |
| 任务清单随版本变化 | 硬编码分类表会过时 | 未知 code 一律归为 `Unforgeable`（失败关闭），日志提示「未识别任务，已跳过」 |
| 专家 id 失效 | 事件被接受但不推进 | id 池可配置；复核进度无变化则换 id 重试一次 |

**伦理/合规提示**：这是自动化"官方设计的用户任务"。参考实现采取的是
"能做就做、不能做就诚实跳过并给深链"的策略，本版沿用。**不做**任何绕过反作弊
（如伪造设备指纹、破解 unforgeable 任务）。

---

## 8. 附录：国际版调研结论（留档，本版不做）

2026-10-25 用真实国际版账号（Google 登录，`offline_access`，365 天令牌）实测：

| 项目 | 结果 |
|---|---|
| `/v2/activity/growth/tasks` | ✅ 200，5 个任务（`first_chat`/`skill_installed`/`wechat_linked`/`expert_summoned`/`template_used`） |
| `/v2/activity/growth/energy` | ✅ 200，balance 0 |
| `/activity/growth/streak` | ✅ 200，days 0 |
| 伪造 `chat_request_send` 上报 | ❌ 200 但**进度不推进**（`available` → `available`） |
| 每日活跃 | 只认"跑完的 agent 会话"，需 ACP over HTTP+SSE（建会话→取沙箱→initialize/load/prompt→completed） |
| 签到 | ❌ 无此机制 |

**结论**：国际版的积分获取全部绑定真实客户端行为信号，程序化上报无效。若要支持，
需实现 ACP over SSE 协议栈，工作量等同新子系统，且稳定性依赖网页端协议——
不适合塞进 2.2.0。

另附国际版有用信息（供未来版本）：
- 聊天/模型路径与国内版**完全一致**（`/v2/chat/completions`、`/v3/config`）
- 模型目录 **29 个**（含 `gpt-5.5`/`gemini-3.8-flash`/`grok-4.7` 等国内版没有的）
- 令牌 365 天且带 `offline_access`，比国内版 `azp=invite` 账号长效得多
- `11-128` 的 msg 是 `request illegal`（国内版是 `Illegal API invocation from an unapproved channel`），
  **语义不同，降级重试逻辑需按 realm 区分**

---

## 9. 验收清单

- [ ] 真实国内版账号跑一次，`energy.balance` 实际增长，日志给出「+N 分」
- [ ] 不可自动化任务（关注公众号等）如实跳过，显示原因与深链，不虚报
- [ ] 上报后进度无变化时，明确记录「上游未推进」而非当作成功
- [ ] 国际版 / 企业号 / 受限账号安全跳过，不产生请求
- [ ] 账号间错峰、单账号预算上限生效
- [ ] `growth_enabled` 默认关闭；开启后每天最多一次
- [ ] `task check` 全绿（fmt / clippy / test / ui-check / ui-smoke）
