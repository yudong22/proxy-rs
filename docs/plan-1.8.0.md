# proxy-rs v1.8.0 功能规划

> 目标版本：1.8.0（当前 1.7.1）
> 三大特性：
> 1. WorkBuddy 源的 Cookie（登录态）登录方法（参考 workbuddy2api）
> 2. 多 Session（多账号/多密钥）下，选中 Session 超限异常时自动切换到另一个 Session
> 3. 请求日志显示请求使用的 Key，并记录该请求是否因异常触发了 override（failover/degraded/session 切换）

---

## 现状摘要（调研结论）

| 领域 | 现状 |
|---|---|
| 上游鉴权 | `Config::api_key` 单一静态密钥 + 可选 passthrough（`src/config.rs`、`src/proxy.rs::resolve_api_key`）；WorkBuddy 走 `x-api-key` + `Authorization: Bearer` 双头 + CLI 指纹（`apply_upstream_auth`） |
| 故障转移 | `forward_request` 多 URL 重试（429/5xx），以及 11128 降级重试、11101 改流式重试；**没有**多账号/多 Key 轮换 |
| 日志 | GUI 环形日志 `LogBuffer` + `~/.proxy-rs/logs/proxy.log`；`stats.db request_logs` 已有 model/route/tokens/status/error/session_id/client 字段（`src/stats.rs`），但**不记录命中的 key，也不记录是否发生过异常 override** |
| GUI | `ui/` 原生 JS，`GuiSettings` 单 key 单 provider；`src-tauri` 侧 `save_settings` / `get_request_logs` 命令 |

### workbuddy2api 的登录/凭据机制（逆向结论，作为参考）

来源：[weecliz/workbuddy2api](https://github.com/weecliz/workbuddy2api)、[hawklithm/workbuddy2api](https://github.com/hawklithm/workbuddy2api)（`codebuddy_client_demo.py` / `backend_profile.py` / `docs/REVERSE_ENGINEERING.md`）

1. **登录态本质**是 `{auth: {accessToken, refreshToken, expiresAt, domain}, account: {uid, enterpriseId, ...}, machineId}` 的 JSON（桌面端 `.info` 文件 / `~/.codebuddy-session.json`），**不是浏览器 Cookie**——"cookie 登录"实为导入登录态（access/refresh token）。
2. **凭据导入渠道**：桌面端登录态 `.info` 文件（Windows: `%APPDATA%/CodeBuddyExtension/Data/Public/auth`）、浏览器登录态、OAuth 设备授权流程（`POST /v2/plugin/auth/state` → 浏览器打开 `authUrl` → 轮询 `GET /v2/plugin/auth/token?state=` → `GET /v2/plugin/auth/login/account?state=`）。
3. **token 刷新**：临近过期时 `POST /v2/plugin/auth/token/refresh`，带 `X-Refresh-Token` 头 + `X-Auth-Refresh-Source: plugin`，成功后回写会话文件。
4. **上游请求指纹头**：`Authorization: Bearer <accessToken>`、`X-User-Id`、`X-Enterprise-Id`、`X-Tenant-Id`、`X-Domain`、`X-Product-Code: codebuddy`、`X-IDE-Type/Name/Version`、`X-Product-Version`、`X-Machine-Id`，风控敏感接口还需 `X-Device-Token`（Turing SDK，缺失可降级但易触发风控）。
5. **账号池实践**：多账号轮换、按剩余额度/LRU 选号、账号级 3 次换号重试、错误分类差异化冷却、超额/限流换号。

---

## Feature 1：WorkBuddy 源 Cookie（登录态）登录

### 1.1 数据模型

新增 `src/workbuddy_auth.rs`（Layer 3，含 I/O，与 credits.rs 同级）：

```rust
pub struct WorkBuddyCredential {
    pub id: String,              // 稳定 id（日志用，如 wb-8f3a1c）
    pub label: String,           // 用户可读标签（如 "工作号"）
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub expires_at_ms: Option<i64>,
    pub domain: Option<String>,  // X-Domain
    pub account: Option<WorkBuddyAccount>, // uid / enterpriseId / nickname
    pub machine_id: Option<String>,
    pub enabled: bool,
    pub cooldown_until_ms: Option<i64>,    // Feature 2 用
    pub last_error: Option<String>,
}
```

- 持久化：`~/.proxy-rs/workbuddy-credentials.json`（权限 0600，参照 workbuddy2api 的 `_save_session`）；跟随 `PROXY_DATA_DIR` 数据目录约定。
- `GuiSettings` 增加 `workbuddy_auth: bool`（是否启用登录态模式）与 `workbuddy_credential_ids: Vec<String>`（启用池）；保留 `api_key` 兼容纯 API Key 模式。

### 1.2 登录态导入（GUI 三种方式）

1. **粘贴 JSON/Token**：粘贴桌面端 `.info` 或 `~/.codebuddy-session.json` 内容（或仅 access/refresh token），解析归一化为 `WorkBuddyCredential`。
2. **扫描本机**：自动发现 WorkBuddy/CodeBuddy 桌面端登录态目录（macOS 路径待逆向确认，先支持手动指定目录 `WB_AUTH_DIR` + 常见默认路径探测）。
3. **OAuth 浏览器登录（P1，可延至 1.8.1）**：实现 `POST /v2/plugin/auth/state` → 打开浏览器 → 轮询 token → `login/account` 的完整流程（移植 `codebuddy_client_demo.py::login`，tokio + reqwest 重写）。

Tauri 命令：`wb_credentials_list / wb_credentials_add / wb_credentials_delete / wb_credentials_toggle / wb_login_start / wb_login_poll`。

### 1.3 鉴权头注入与 token 刷新

- 扩展 `apply_upstream_auth`（`src/proxy.rs`）：WorkBuddy flavor 且启用登录态时，注入完整指纹头组（`X-User-Id` / `X-Enterprise-Id` / `X-Tenant-Id` / `X-Domain` / `X-Product-Code` / `X-IDE-*` / `X-Machine-Id` + `Authorization: Bearer <accessToken>`），替代现有 `x-api-key` 双头。
- 新增 `wb_refresh_token`：401 或 `expiresAt` 临期（< 60s）时调 `/v2/plugin/auth/token/refresh`（带 `X-Refresh-Token`），成功回写凭据文件；失败标记凭据 `last_error` 并进入冷却。
- `credits.rs` 的额度查询同步支持登录态鉴权（当前走 `X-API-Key`）。

### 1.4 测试

- 单测：`.info`/session JSON 各版本形态解析（含 `data.data` 解包）、头组构造快照测试、过期判定、刷新请求体/头快照。
- 集成：mock 上游 401 → 刷新 → 重放原请求。

---

## Feature 2：多 Session 超限异常自动切换

### 2.1 概念与选型

- "Session" = 一个可用上游凭据单元（WorkBuddy 登录态凭据，或多个 `UPSTREAM_API_KEYS` 中的普通 Key）。
- 触发切换的"超限异常"（可配置，默认全开）：
  - HTTP 429 / 402（额度耗尽，workbuddy2api 对超额固定 402）
  - WorkBuddy 业务码：额度/限流类（如 `11105`/`11106` 类配额码，按实测补充；`11128` 内容审核**不**切换账号——换号无效，保持现有降级重试）
  - 401 且 refresh 失败（凭据失效）
- 新增 `src/session_pool.rs`（Layer 3）：

```rust
pub struct CredentialPool {
    entries: RwLock<Vec<PoolEntry>>,   // 凭据 + 状态（健康/冷却/禁用/剩余额度）
    /// 仅记录因 4xx 被切换过的会话：session_id -> 切换后凭据 id。
    /// 从未出错的会话不进此表，始终走默认源。
    switched: RwLock<LruCache<String, String>>,
}
impl CredentialPool {
    /// 选号：该会话曾因 4xx 切换过且目标凭据仍健康 → 用切换后的凭据；
    /// 否则一律走默认（选中）凭据。默认凭据健康时零查表开销。
    pub async fn pick(&self, session_id: &str) -> String;
    /// 该会话遇 4xx 时调用：从健康凭据中选一个（剩余额度最多优先，其次轮转），
    /// 记入 switched 表并返回新凭据。
    pub async fn switch(&self, session_id: &str, from: &str) -> Option<String>;
    pub async fn mark_limited(&self, id: &str, cooldown: Duration); // 记异常+冷却
    pub async fn mark_ok(&self, id: &str);
}
```

### 2.1a 切换后会话粘滞（failover stickiness）——仅 4xx 触发

**选号语义（与负载均衡划清界限）**：

1. **默认源优先，无错不切**：GUI 选中的默认凭据健康时，**所有**会话的所有请求都走它——没有主动分散、没有"剩余额度优先"的日常选号。`pick()` 对从未出错的会话只是直接返回默认凭据。
2. **仅 4xx 触发切换**：只有某个会话的请求实际命中超限/失效类异常（429/402/配额业务码/401 且 refresh 失败）时，才为**这一个会话**调用 `switch()` 选备用凭据；其他会话不受影响，继续走默认源。
3. **切换后粘滞（为了缓存）**：一旦切换，该会话记入 `switched` 表，后续请求持续使用切换后的凭据——这样同一会话在新凭据上重建并命中 prompt cache，而不是逐请求轮转打碎缓存。切换事件即 Feature 3 的 `session_switch` override。
4. **粘滞回收**：`switched` 表是进程内 LruCache（默认 1000 条 / TTL 24h，`PROXY_SESSION_AFFINITY_TTL` 可配），LRU 淘汰最长未活跃的已切换会话；**不持久化**——重启后回到"全部走默认源"的初始状态，只损失一次缓存预热。默认凭据冷却到期恢复健康后，已切换会话**保持**粘在新凭据上（避免反复横跳使缓存反复失效）；GUI 提供"重置粘滞"按钮让全部会话回到默认源。
5. **切换时的备选顺序**：剩余额度最多优先（WorkBuddy 凭据能查到额度时）→ 其余轮转。这只在故障切换瞬间使用，不是日常策略。
6. **无会话键的请求**（`client=unknown`，如 curl/裸 API）：始终走默认源；出错时允许请求级重试切到备用凭据，但不记粘滞（下次请求仍从默认源开始）。
7. **开关**：`PROXY_SESSION_SWITCH`（默认 `true`）+ GUI 开关；关闭后遇 4xx 不切换、直接把上游错误透传给客户端，行为同 1.7.1。
8. **缓存收益验证**：利用现有 `cache_read_tokens` 统计——被切换过的会话在新凭据上连续请求的 `cache_read` 占比应显著上升（见验收清单）。

- 每个 URL 的重试预算不变：单凭据内仍是"原请求 + 最多 1 次恢复重试"，单请求跨凭据最多切换 `min(池大小-1, 2)` 次，避免放大风暴（对齐 workbuddy2api 的"账号级 3 次换号重试"）。

### 2.2 forward_request 改造

- 选号在进入 URL 循环前完成：`let key_id = pool.pick(&session.session_id).await`，粘滞凭据不可用时内部完成 `switch()`；内层保留现有 `'url` × `attempt` 循环。每次切换记一条 `WARN` 日志：`session 切换 from=<id> to=<id> reason=429/402/111xx session=<session_id>`，并写入 `switched` 表。
- 切换对客户端完全透明（同一路由、同一响应格式）；全部凭据耗尽时返回最后一次的真实上游错误（便于排查）。

### 2.3 配置与 GUI

- 普通 Key 多账号：`UPSTREAM_API_KEYS`（分号分隔，与 `UPSTREAM_BASE_URL` 语义对齐）；WorkBuddy 登录态凭据池见 Feature 1。
- GUI「厂商设置」页新增：凭据池列表（启用/禁用/**设为默认源**/显示冷却状态/**当前粘滞会话数**）、"超限自动切换"开关（`PROXY_SESSION_SWITCH`）、粘滞 TTL 输入框、"重置粘滞"按钮。

---

## Feature 3：请求日志显示 Key 与 override 记录

### 3.1 stats.db 扩展（`request_logs` 迁移）

按现有 `migrate()` 的 `ALTER TABLE ADD COLUMN` 模式**只新增 2 列**：

| 列 | 类型 | 说明 |
|---|---|---|
| `override_key` | TEXT | 发生异常 override 时实际使用的凭据（sessionkey）短 id；未发生 override 时为空。即该请求因异常切换/降级后"保存下来"的那个 key |
| `override_model` | TEXT | 发生异常 override 时选择的模型名（如降级重试/切换后的上游模型）；默认为空字符串 |

- 请求来自哪个 key 不落库：GUI/文件日志行的 `key=<id>` 已实时展示，DB 只保留 override 追溯所需的最小信息。
- 旧数据两列以空字符串填充，无需回填。

### 3.2 代码路径

- `RequestOutcome` 增加 `override_key` / `override_model` 两个字段；`forward_request` 内部用一个小的 `OverrideTrace` 结构在整条重试链上累计事件（触发原因 + 切换后的 key/model），最终只把这两个归约值随结果一并写入（三处 handler 的 `finalize_request` 同步透传）。
- **成功请求也要落库**：当前成功路径只在 GUI 日志打一行、不写 `request_logs`——1.8.0 起成功请求同样写入（带 tokens），否则"因 override 而成功的请求"无从追溯（这类请求正是 `override_key` 非空的主体）。
- 日志行：GUI/文件日志的 `POST ... ok/failed` 行尾追加 `key=<id>`（本次命中的凭据），发生 override 时加 `override=<kind>(<reason>)`；`override_key`/`override_model` 仅入库，不重复打印。
- 日志与 `metrics` 严禁输出完整 token——只输出凭据短 id 与 label。

### 3.3 GUI 展示

- 「请求日志」表格新增"Override Key"与"Override Model"两列（均为空时显示"—"，表示该请求未发生 override）；详情展开显示 override 原因摘要（如 `429 → key-2 / deepseek-v4.1`）。

---

## 实施顺序与里程碑

| 阶段 | 内容 | 依赖 |
|---|---|---|
| M1 | Feature 3 的 stats 列扩展 + `OverrideTrace` + 成功请求落库 + GUI 日志列（不含 session_switch 事件） | 无 |
| M2 | Feature 1 凭据模型/存储/头注入/token 刷新 + GUI 凭据管理（导入粘贴 + 扫描本机） | 无 |
| M3 | Feature 2 `CredentialPool`（默认源优先 + 4xx 切换粘滞）+ forward_request 多凭据支持 + 切换事件接入日志（补 Feature 3 的 `session_switch`） | M1、M2 |
| M4 | OAuth 浏览器登录流程、credits 接入登录态、文档（README 环境变量表、troubleshooting）、`task check` 全绿 | M2 |

## 风险与注意

1. **风控**：登录态模式必须带上完整指纹头组；`X-Device-Token`（Turing SDK）在 Rust 侧无法直接复用原生模块，1.8.0 先降级不带（workbuddy2api 证实可跑但更易触发风控），在文档中注明风险；观察是否需要后续接入外部 helper 进程方案。
2. **token 安全**：凭据文件 0600；日志/遥测/错误信息里只允许出现凭据短 id。
3. **版本耦合**：上游 UA `CLI/unknown CodeBuddy/2.137.1` 与指纹头版本号随官方客户端升级会漂移，需集中到 `providers.rs` 常量并在 README 注明同步方法。
4. **兼容性**：未配置凭据池时行为与 1.7.1 完全一致；`request_logs` 旧数据新列以空值填充。
5. **11128 语义**：内容审核拦截换号/换配置均无效（workbuddy2api 明确提示），保持现有 degraded 重试，不纳入换号触发条件。

## 验收清单

- [ ] 粘贴登录态 JSON 后，代理以其身份成功完成 `/v1/messages` 流式对话，日志可见 `key=wb-xxxx`
- [ ] access token 过期后请求自动刷新并成功，凭据文件被回写，日志出现 `override=token_refresh`
- [ ] 配置 2 个凭据，第 1 个触发 429/402 后自动切换第 2 个完成请求，日志/请求日志可见 `override=session_switch(...)`，第 1 个进入冷却
- [ ] **默认源优先**：默认凭据健康时，所有会话（含多个并发会话）的请求全部走默认源（单测断言 `pick()` 恒返回默认凭据，日志 `key=` 一致），无任何主动分散
- [ ] **4xx 触发切换**：仅命中 429/402/配额码/refresh 失败的会话被切换，其他会话不受影响；`session_switch` 只出现在出错会话的日志里
- [ ] **切换后粘滞**：被切换会话的后续请求持续走新凭据（日志 `key=` 稳定），新凭据上 `cache_read_tokens` 占比明显上升（手工验收：Claude Code 连续多轮对话，检查 stats.db）
- [ ] **粘滞回收**：LRU/TTL 淘汰后该会话回到默认源；重启后全部会话走默认源；GUI"重置粘滞"后立即回到默认源
- [ ] 关闭 `PROXY_SESSION_SWITCH` 后遇 4xx 不切换、错误原样透传，行为同 1.7.1
- [ ] 全部凭据不可用时返回真实上游错误，GUI 有明确提示
- [ ] 请求日志页可见 Override Key / Override Model 两列：未 override 的请求两列为空；发生 override 的请求能追溯原因摘要；无任何完整 token 泄漏到日志
- [ ] `cargo test` + 现有集成测试全绿；未配置新特性时行为与 1.7.1 一致
