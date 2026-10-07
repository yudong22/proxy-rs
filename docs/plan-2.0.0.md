# proxy-rs v2.0.0 功能规划：账号池导入 / 导出

> 目标版本：**2.0.0**（当前 1.9.13）
> 特性：把「身份池」（WorkBuddy 登录态账号 + 上游 API 密钥 + 默认身份/打卡偏好）导出为一个可携带文件，并在新机器上一键导入。

---

## 1. 目标与非目标

### 1.1 目标

| # | 目标 | 验收方式 |
|---|---|---|
| G1 | 一键导出全部账号与密钥，得到一个自包含文件 | GUI「导出账号池」→ 文件落盘，权限 0600 |
| G2 | 新机器上一键导入，导入后立即可用（无需重启应用） | 导入后请求日志出现 `key=<导入的 id>` |
| G3 | 明文与加密两种形态，由用户选择 | 加密包在无口令 / 错口令时给出明确错误 |
| G4 | 导入不破坏本机已有数据，且可预知影响 | 导入前预览（新增/覆盖/跳过/警告），写入前自动备份 |
| G5 | 全流程不泄漏 token 到日志、UI 与导出摘要 | 日志仅出现短 id；UI 只显示 `masked_token` |

### 1.2 非目标（本版不做）

- **不导出** `stats.db`（历史请求日志可能上百 MB，且与"账号池"是两件事）。整机迁移另做。
- **不导出** `gui-settings.json` / `.env` 中的端口、绑定地址、模型映射、指纹清洗短语。这些是**机器相关**配置，导入到另一台机器往往有害（端口冲突、上游地址错位）。仅导出身份池自身需要的偏好（默认身份、定时打卡）。
- **不做** 云端/局域网同步、二维码直传。文件即载体。
- **不改** 三个状态文件的既有磁盘格式（`workbuddy-credentials.json` / `api-keys.json` / `workbuddy-pool.json` 仍原样），导入/导出只是在其上加一层传输格式。

---

## 2. 现状摘要（调研结论）

| 领域 | 现状 |
|---|---|
| 账号池存储 | `~/.proxy-rs/workbuddy-credentials.json`（登录态，0600）、`~/.proxy-rs/api-keys.json`（上游密钥，0600）、`~/.proxy-rs/workbuddy-pool.json`（默认身份 `default_identity_id` + 定时打卡），全部经 `settings::data_dir()` 随 `PROXY_DATA_DIR` 重定位（`src/workbuddy_auth.rs:892/574/114`）；默认身份哨兵 `__none__` 见 `src/workbuddy_auth.rs:728` |
| id 稳定性 | `credential_id()` / `api_key_id()` 是**内容哈希**（`wb-<6hex>` / `k-<6hex>`）：同一 token 在任意机器上 id 相同 → 跨机合并天然可去重（`src/workbuddy_auth.rs:309/561`） |
| 一次性导入 | 已有「粘贴登录态 JSON」单条导入（`parse_login_state` + `wb_credentials_add`）、单条密钥添加（`api_keys_add`）。**没有**批量、没有导出 |
| 热生效 | 密钥：`Config::api_key` 每次构建配置时读 `active_api_key()`（`src/config.rs:235`）→ 立即生效。登录态：需 `reload_pool()` + `start_proxy_server()`（见 `wb_set_default_identity`，`src-tauri/src/main.rs:997`） |
| GUI | 身份池工具栏在 `ui/src/views/settings/form.js`（`.wb-pool-toolbar`），交互在 `ui/src/views/settings/credential-pool.js`，样式在 `ui/style/workbuddy.css`；前端**无打包器**，模态由 `components/modal.js` / `overlay.js` 按需创建 |
| 文件对话框 | 未安装 `tauri-plugin-dialog`；`capabilities/default.json` 只授了 `core:default`。现有唯一的"落盘后告知"先例是 `open_logs_dir`（用 `open` / `xdg-open` / `explorer` 打开目录，`src-tauri/src/main.rs:1473`） |
| 质量门禁 | `task check` = `cargo fmt --check` + `clippy -D warnings` + `cargo test` + `ui-check`（静态 id/导入图）+ `ui-smoke`（无头 Chrome 对 mock 后端跑真实页面） |

**结论**：不需要新的存储层，也不需要改动池的运行时逻辑。需要的是一个**传输格式 + 校验/合并/落盘模块**，加上 GUI 的两个模态与三条命令。

---

## 3. 传输格式规范

### 3.1 明文包（默认）

```jsonc
{
  "format": "proxy-rs-pool",          // 固定魔数，导入时校验
  "version": 1,                        // 传输格式版本，与 App 版本解耦
  "kind": "plain",
  "exported_at_ms": 1759212345678,
  "app_version": "2.0.0",              // 仅供人读与排障
  "counts": { "credentials": 2, "api_keys": 1 },
  "checksum": "9f2c…",                 // 见 3.3
  "preferences": {
    "default_identity_id": "wb-8f3a1c",
    "daily_checkin_enabled": true,
    "daily_checkin_time": "09:00"
  },
  "credentials": [ /* Vec<WorkBuddyCredential> 原样 */ ],
  "api_keys":    [ /* Vec<ApiKeyEntry> 原样 */ ]
}
```

- `credentials` / `api_keys` 直接复用 `WorkBuddyCredential` / `ApiKeyEntry` 的 `Serialize`/`Deserialize`，**不另建 DTO**：避免两套字段定义漂移。新增字段时用 `#[serde(default)]` 的老字段自然兼容旧包。
- 导入时**忽略**未知字段（serde 默认行为），因此高版本导出的包在低版本上仍能导入可用部分。

### 3.2 加密信封（可选口令）

勾选口令后，整体变为信封结构：`credentials` / `api_keys` / `preferences` 全部进入密文，**外层不残留任何 token**。

```jsonc
{
  "format": "proxy-rs-pool",
  "version": 1,
  "kind": "encrypted",
  "exported_at_ms": 1759212345678,
  "app_version": "2.0.0",
  "counts": { "credentials": 2, "api_keys": 1 },   // 为导入预览保留的明文计数，不含敏感值
  "kdf":    { "algo": "argon2id", "m_cost": 19456, "t_cost": 2, "p_cost": 1, "salt_b64": "…" },
  "cipher": { "algo": "aes-256-gcm", "nonce_b64": "…", "ct_b64": "…" }
}
```

- **KDF**：Argon2id（`m=19456 KiB (19 MiB)`, `t=2`, `p=1`），16 字节随机盐；输出 32 字节密钥。参数写入文件，便于后续调参而不破坏旧包。
- **AEAD**：AES-256-GCM，12 字节随机 nonce。GCM 的 tag 即完整性校验，密文被改动 → 解密失败（报「口令错误或文件已损坏」，不区分二者）。
- **依赖**：`argon2` + `aes-gcm` + `base64`（纯 Rust，无 C 依赖；`Cargo.lock` 中已有 `base64`，仅需提升为直接依赖）。
- **明文包与加密包都是 UTF-8 JSON 文本**：便于用户自己查看/备份/用 `python -m json.tool` 排障，加密包也只需一个口令而非专有工具。

### 3.3 校验与导出归一化

导出前对每条记录做**归一化**，把"本机运行时状态"与"身份本身"分开：

| 字段 | 导出处理 | 理由 |
|---|---|---|
| `id` / `label` / `access_token` / `refresh_token` / `expires_at_ms` / `domain` / `account` / `machine_id` | **保留** | 身份本体；`machine_id` 保留可让上游看到同一设备，降低风控概率 |
| `enabled` | 保留 | 禁用是用户的明确意图，应随包转移 |
| `points` / `points_fetched_at_ms` | 保留 | 新机器首屏不至于显示"未知"，且带抓取时间可判断新鲜度 |
| `last_checkin_date` | 保留 | 避免导入当天重复打卡 |
| `cooldown_until_ms` | **清空** | 冷却源于旧机器/旧网络的一次限流，新机器继承它没有意义 |
| `last_error` | **清空** | 同上，且旧错误文本会误导排障 |

`checksum` = 对**整个 payload**（`preferences` + `credentials` + `api_keys`，归一化之后）的规范 JSON 取 MD5（复用 `workbuddy_auth::md5_hex`，不引入 crypto 依赖——这是**防手改/防截断**的指纹，不是密钥派生）。导入时在归一化改写任何字段之前先校验：不匹配则记 `warning` 但**继续**导入（用户可能手工编辑过包），并在预览里显式提示；只有 JSON 解析或解密失败才终止。

### 3.4 导入合并策略

导入者显式二选一：

| 模式 | 语义 |
|---|---|
| **合并**（默认） | 按 id 合并。本机不存在的 id → 新增；已存在的 id → 以包内记录为准覆盖 `token`/`label`/`enabled`/`points`，但**保留本机更新的 `refresh_token`/`expires_at_ms`**（若包内该字段缺失） |
| **替换** | 清空本机两个池，完全以包内容为准（导入前自动备份原文件） |

两条共同规则：

1. **默认身份**：包内 `default_identity_id` 指向的 id 若在导入后存在 → 应用；否则清空（回退到「账号优先，否则密钥 / 单 Key」）并记 `warning`。**永不让池指向不存在的 id**。
2. **定时打卡**：`daily_checkin_enabled` / `daily_checkin_time` 仅在导入包含该字段时覆盖（`Option` 语义）；`last_checkin_run_date` 不导入——它属于本机调度状态。
3. **原子写入 + 备份**：三个文件各先写 `<file>.proxy-rs-backup-<stamp>`（沿用 `dsh_config.rs:985` 的 `backup_stamp()` 风格），再写临时文件 `rename` 覆盖。任一文件失败即中止并报告，不留半个池。

---

## 4. 架构与模块划分

新增 **`src/pool_transfer.rs`**（Layer 3，含 I/O，与 `credits.rs` / `workbuddy_auth.rs` 同级；不进入纯函数转换核心，符合 `specs/architecture.md` 的分层约束）：

```rust
pub const BUNDLE_FORMAT: &str = "proxy-rs-pool";
pub const BUNDLE_VERSION: u32 = 1;
pub const MAX_BUNDLE_BYTES: usize = 1024 * 1024;

pub struct ExportOptions { pub passphrase: Option<String> }
pub enum ImportMode { Merge, Replace }        // ImportMode::parse(&str) 供命令层翻译 GUI 参数

/// 汇总本机身份池 → 归一化 → 编码为可携带文本。
pub fn export_text(opts: &ExportOptions) -> Result<(String, ExportOutcome)>;

/// 解析（必要时解密）→ 校验 → 只读比对本机现状，不写盘。
pub fn preview_import(text: &str, passphrase: Option<&str>, mode: ImportMode) -> Result<ImportPreview>;

/// 落盘（含备份），返回与 preview 同形的结果供 UI 展示。
pub fn apply_import(text: &str, passphrase: Option<&str>, mode: ImportMode) -> Result<ImportPreview>;

/// 默认导出路径：`~/Downloads/proxy-rs-pool-YYYYMMDD.json`（无下载目录时回退数据目录）。
pub fn default_export_path() -> PathBuf;

/// 用户自定义文件名 → 清洗后的 `<dir>/<name>.json`（只允许基名，杜绝路径穿越）。
pub fn export_path_in(dir: &Path, filename: Option<&str>) -> PathBuf;
pub fn sanitize_filename(raw: &str) -> String;

/// 原子写入导出文件（0600）。
pub fn write_bundle(path: &Path, text: &str) -> Result<()>;
```

> 与原规划的两处偏差（实现后回填）：`ImportOptions` 被拆成两个参数——只有一个 `mode` 与一个 `passphrase`，包一个结构体只是多一层；`default_export_path` 自己取当前时间，不再由调用方传 `stamp`（唯一调用点是命令层，传参只会多一个可能传错的地方）。

要点：

- **编码/解码与写盘分离**：`preview_import` 是纯读路径（可反复调用），GUI 的「预览 → 确认」两步因此不共享可变状态；`apply_import` 重新解析一次文本而不是复用预览结果，避免"预览后文件被换掉"的 TOCTOU。
- **口令只存在于栈上**：不写日志、不进 `LogBuffer`、不序列化。
- **`ImportPreview`** 字段：`credentials_added/updated/unchanged/removed`、`keys_added/updated/unchanged/removed`、`default_identity_applied: String`、`default_identity_dropped: bool`、`warnings: Vec<String>`（`applied`/`dropped` 用 `String`/`bool` 而非 `Option`：GUI 直接把它当文本渲染，"未变更"与"被清空"是两种要分开显示的文案）。
- 复用而非重写：`load_credentials` / `save_credentials` / `load_api_keys` / `save_api_keys` / `load_preferences` / `save_preferences` / `credential_id` / `api_key_id` / `md5_hex`（后者改为 `pub(crate)`）。

**不新增状态文件**，因此 `settings::data_dir()` 的"一个旋钮搬走全部状态"约定不被破坏。

---

## 5. 后端命令（`src-tauri/src/main.rs`）

| 命令 | 入参 | 返回 | 说明 |
|---|---|---|---|
| `pool_export` | `{ passphrase?: string, filename?: string }` | `{ ok, cancelled, path, encrypted, counts }` | 弹原生「另存为」面板（`filename` 仅作预填建议）→ 用户选定后才做 KDF 与写盘；取消返回 `{ ok: false, cancelled: true }` |
| `pool_import_preview` | `{ text, passphrase?, mode }` | `ImportPreview` | 只读；不落盘 |
| `pool_import` | `{ text, passphrase?, mode }` | `ImportPreview` | 落盘 + `reload_pool()` + `start_proxy_server()`（若在运行）+ 日志一行（仅计数与短 id） |
| `reveal_path` | `{ path }` | `{}` | 在访达/资源管理器/文件管理器中显示；把现有 `open_logs_dir` 的平台分支提取成 `reveal_in_file_manager(path)` 复用（macOS 对文件用 `open -R` 选中而非打开） |

- **`pool_export` / `pool_import` / `pool_import_preview` 走 `spawn_blocking`**：整包读取、Argon2id 派生与三文件写入都是阻塞操作，直接在 async 命令里跑会占住 webview 其他命令共享的工作线程——与仓库既有的 `get_stats` / `get_request_logs` 约定一致。
- 命令名不带 `wb_` 前缀：它同时覆盖密钥池，且未来可能扩展到其他池；GUI 内部仍在 `wb-` 前缀的 id 命名空间下（见 6.2）。
- 导出的文件名做了**白名单清洗**（取最后一个路径分量，只保留字母/数字/`-`/`_`/`.`，其余替换为 `-`；中文文件名仍然可用），用户**无法决定写入目录**，从根上杜绝 `../` 穿越。
- 导入的 `text` 有大小上限（1 MiB），超出直接报错，避免异常输入撑爆内存。

---

## 6. GUI 设计

### 6.1 交互

身份池工具栏新增两个按钮（与现有 `📱 扫码添加账号` 同排）：

- **`📤 导出账号池`** → 模态：显示`账号 N 个 · 密钥 M 个`，可选「使用口令加密」勾选 + 口令输入（两次确认），可选文件名，`导出` 按钮 → 成功后就地显示完整路径 + 「在文件夹中显示」。
- **`📥 导入账号池`** → 模态：
  1. 顶部提示："导出文件内含可直接使用的登录态与密钥，请只在自己的设备间传递"；
  2. 来源二选一：**选择文件**（`<input type="file" accept=".json,application/json">` + `FileReader`）或**粘贴 JSON**（`<textarea>`，与既有「粘贴登录态」交互一致，也是 webview 文件选择的兜底）；
  3. 口令输入（仅加密包需要，留空即按明文解析）；
  4. 合并策略（单选：合并 / 替换）；
  5. `预览` → 渲染 `ImportPreview`（新增/覆盖/跳过计数 + 警告列表）；
  6. `确认导入`（危险操作，走 `components/confirm.js` 二次确认，**替换模式**额外强调"将清空本机现有账号池"）。

导入成功后：关闭模态 → `renderIdentityPool()` → `refreshStatus()` → toast + 身份池状态行提示「导入的登录态可能需要重新打卡 / 刷新积分」。

### 6.2 模块与约定落点

新增 **`ui/src/views/settings/pool-transfer.js`**（导出 `initPoolTransfer()` / `openExportDialog()` / `openImportDialog()`），`views/settings/index.js` 负责 `initPoolTransfer()` 的装配（沿用"视图只渲染、main/index 装配"的既有约定）。

> 实现偏差：按钮接线也放在 `pool-transfer.js` 自己身上（`initPoolTransfer` 直接绑 `#btn-wb-export` / `#btn-wb-import`），`credential-pool.js` 完全不改。原计划"在 credential-pool.js 里把按钮接上"会制造一个反向 import（池模块 → 传输模块 → 池模块，因为传输完成要重绘池），而这正是 `ui/README.md` 第 5 条禁止的循环依赖；`pool-transfer.js` 单向依赖 `credential-pool.js`（用它的 `renderIdentityPool` / `showWbStatus`）就够了。
> 另外两个弹窗共用 `components/modal.js` 的**同一个** overlay 实例（`#request-modal-overlay`），因此各自在 `openModal(title, '')` 之后用 `setModalBody()` 整体替换 body 并重写 footer——复用 shell 而不是新建第二个模态，符合"同一外观只有一处定义"。

- **id 前缀**：沿用 `wb-`（该模块属于身份池）：`btn-wb-export`、`btn-wb-import`、`wb-transfer-*`。
- **CSS**：全部写进既有的 `ui/style/workbuddy.css`，**不新增样式表**（`check-ui.mjs` 会拦截未登记的 `<link>`）；颜色只取 `tokens.css` 令牌，模态复用 `.modal-overlay/.modal-card` 与 `.btn` 层级，不新开一套卡片样式。
- **零新前端依赖**：不引入 `@tauri-apps/plugin-dialog`（无打包器，装不进 `ui/`）；**导入**侧的文件选择与读取用平台原生 `<input type="file">` + `FileReader`，后端只收文本。

### 6.3 导出改用原生「另存为」面板（2.0.0 增补）

初版把导出目录固定在 `~/Downloads`、用户只能改名。这在新机器上不好用（可能没有 Downloads、也可能想直接存到 U 盘或加密卷），因此导出改为调用系统的保存面板：

- **依赖**：`tauri-plugin-dialog`（src-tauri 侧，仅 Rust 使用）。`ui/` 仍然零新依赖。
- **权限**：`capabilities/default.json` **刻意不加** `dialog:default`。插件虽然注册了 webview 可调用的 `open`/`save`/`message` 命令，但不授权就等于前端无法触达——面板只由 Rust 打开，**用户选定的路径从不经过 JS**。
- **顺序**：先弹面板，用户选定后才做池读取 → Argon2id → 写盘。取消因此不浪费 KDF 时间，口令也不会在文件落点确定前离开进程。
- **不用 `blocking_save_file`**：该辅助函数以 `rx.recv().unwrap()` 收尾，而 `save_file` 会丢弃内部 `run_on_main_thread` 的错误。派发失败 → sender 被丢弃 → `recv` 返回 `Err` → **panic**；release profile 是 `panic = "abort"`，一个没弹出来的对话框会直接终止整个应用。改为回调 + 本地 channel，同样的失败退化成 `None`，而调用方本就把 `None` 当作「已取消」。
- **取消是正常结果**：命令返回 `{ ok: false, cancelled: true }`，UI 显示「已取消导出（未写入任何文件）」，不带错误色，按钮可再次点击。取消**不写日志**（用户已经知道，写一行只是噪音）。
- `export_path_in` / `sanitize_filename` 并未失效：它们现在决定面板的**起始目录与预填文件名**。仍然清洗名称，是为了让粘贴进来的 `../../foo` 不会以路径穿越的样子出现在名称输入框里。

### 6.5 「导出内容为空」不再冒充成功（2.0.0 修复）

实测踩到：在 `task dev` 的隔离实例里点导出，得到一个 356 字节、`counts` 全为 0 的文件，而 UI 却报「已导出 0 个账号、0 个密钥」——看起来像成功。这不是壳的问题，是**导出路径上的两个真实缺陷**：

1. **空池导出被当作干净成功。** `pool_export` 对空池照样返回 `ok: true`，用户于是会把这个文件当成备份带到新机器。
2. **算出来的警告被丢掉。** `export_text` 里写的是 `let (payload, _) = collect_payload();` —— `collect_payload` 明明构造了「本机身份池为空」的警告，却被 `_` 吞掉，永远到不了 GUI。
3. **更危险的一条：损坏的文件与空池无法区分。** `load_credentials()` / `load_api_keys()` / `load_preferences()` 都是 `serde_json::from_str(...).unwrap_or_default()`。这对**请求路径**是对的（文件坏了也必须继续服务），但它让「文件损坏」和「池是空的」在调用方看来一模一样 —— 而导出恰恰是最不能混淆这两者的调用方：它会写出一个格式合法、内容为空、看起来能用的"备份"。

修法：

- `ExportOutcome` 增加 `warnings` 与 `is_empty()`；`export_text` 不再吞掉 `collect_payload` 的警告。
- 新增 `workbuddy_auth::store_health()` → `StoreState { Missing, Ok, Corrupt(原因) }`，在读取**之前**区分三态。导出遇到 `Corrupt` 时把**具体原因与文件路径**写进警告；其余健康的部分照常导出（一个文件坏掉不该让整包都空）。
- 命令层：空导出记 `WARN` 日志并回传 `empty: true`；非空才记 `INFO`。
- 前端：`empty: true` 时用错误色显示「⚠️ 导出的文件**不包含任何账号或密钥**（原因）」，并提示可能是在开发实例里导出；toast 也是错误色。**不再走成功提示**。
- 导出弹窗新增两行信息，把这个问题变成点之前就能看见的：**「导出数据目录」**（来自 `get_status().data_dir`，dev 构建额外挂「开发实例」pill）与**「本次将导出：账号 N 个、密钥 M 个」**（复用身份池那两个 list 命令，保证与列表一致；为 0 时标红并加一句说明）。

测试：新增 3 个单测——`an_empty_pool_exports_a_bundle_but_says_so`（文件仍写出、`is_empty()` 为真、警告存在、且空包仍可往返导入）、`a_corrupt_store_is_reported_instead_of_exporting_silence`（截断凭据文件 → 该部分为 0 但警告点名「账号池文件已损坏」，同时**健康的密钥池仍被导出**）、`store_health_separates_missing_from_corrupt`（三态区分）。smoke 增加空导出断言（错误色 + 关键文案子串）与「弹窗显示数据目录 / 数量」断言。

---

## 6.4 云端同步（**已搁置**，等有服务端 API 再做）

曾规划「用数字密码把账号池存到腾讯云 COS、并在新机器上凭数字密码恢复」。结论是**暂不做**：这需要一个自建中转服务才成立，而"为了一个迁移功能在项目里引入服务端"会把项目性质改变。等以后有了服务端 API，再按下面已确认的决定实施。

**当时确认的决定（留给以后接服务端时直接照做）**

| 议题 | 决定 |
|---|---|
| 新机器如何取得 COS 凭据 | **不做内置密钥**。客户端只向服务端出示数字密码；COS 的 SecretId/SecretKey 只存在于服务端。（"每台机器配一次 COS"与"应用内置子账号密钥"都已否决：前者仍要配置、后者等于把密钥随 `.app` 分发。） |
| 数字密码 | **6 位数字**。它是唯一秘密，因此服务端必须配套：Argon2id（成本不低于当前的 19 MiB / 2 轮）、**按密码/IP 的尝试次数限制与锁定**、以及短生命周期。6 位数字（10⁶）在离线场景下不足以对抗爆破，只有服务端限流 + 短期存在才成立。 |
| 对象保留 | **覆盖式 + 自动过期**：同一密码始终写同一个对象，只保留最新一份，并由桶生命周期规则在 N 天后自动删除。一个密码对应一次迁移，用完即弃。 |
| 服务端职责 | 校验密码 → 生成/返回 COS 预签名 URL（或直接中转读写）；**永远不把长期密钥下发给客户端**。 |

**为什么当时没有继续**：若不走服务端，客户端就必须自己持有 COS 的长期密钥才能"只输数字密码就恢复"，而这把密钥可以从 `.app` 里提取；即便用桶侧最小权限（只允许某个前缀）与生命周期规则兜底，也仍然是一个把凭据分发到每台机器的设计。既然你已经明确"不做服务端，以后再说"，这部分就只留决定、不写代码。

### 6.6 账号稳定身份与登录态自愈（2.0.1）

**背景**：`id` 原本是 `hash(access_token)`，而 `access_token` 每次刷新都会变。这个组合有两个后果，都在排查「导出的账号很少 / 401 后无从恢复」时被实测确认：

1. **导入会打乱已在运行的账号**。`reconcile_ids` 强制 `id == hash(access_token)`，于是任何**刷新过一次**的账号都被判定为"不一致"并被改写 id；同时 `default_identity_id` 仍指向旧值，导入后默认身份被静默清空。（实测：账号 id 被改写 + `default_identity_id` 变为空串。）
2. **重新登录会产生重复账号**。`upsert_credential` 按 id 匹配，而重新登录会算出新 id，于是同一账号变成两行、三行。

**改动**：

- **身份改为 `account.uid` + `enterpriseId`**（`identity_key` / `credential_id_for`）。enterpriseId 必须参与，否则同一 uid 的个人号与企业号会被合并。无 uid 时回退 token 哈希（保持对旧 fixture 兼容）。`parse_login_state`、OAuth 回写、导入三条路径统一走这个派生。
- **启动时一次性迁移** `migrate_credential_ids()`：把存量 token 哈希 id 重新按 uid 计算，**并把 `default_identity_id` / `default_credential_id` 通过同一张改名表一起重映射**（而不是让它悬空），重复账号按最近使用合并。幂等。**用真实数据副本验证**：4 个账号全部重键、无重复、默认身份 `wb-4dcc79 → wb-9f667c` 正确跟随、token 未变、二次运行改动为 0。
- **`upsert_credential` 按身份匹配**（`find_index`：先 id 后账号身份），命中时**保留磁盘上的 id**，这样日志、override 归属与默认身份都还能解析到它。
- **`reconcile_ids` 不再改写 id**，改为按稳定身份重键**并重映射默认身份引用**；合并路径也改用 `find_index`，让"本机旧的 token 哈希条目"与"包里 uid 键条目"识别为同一账号。

**主动续期**：真实登录态普遍**没有 `expiresAt`**，而 `needs_refresh` 对 `None` 一律返回 `false` —— 也就是说最需要续期的账号永远不会被提前续期，只能等 401 把用户请求打挂。新增 `last_refresh_at_ms` + `UNKNOWN_EXPIRY_REFRESH_AFTER_MS`（6 小时）兜底：只在"距上次续期已超过窗口"时才主动续期，**绝不覆盖已知的 `expiresAt`**，从未续期过的新导入账号也不触发（它刚由客户端签发）。每次成功刷新都会打时间戳，否则兜底永不前进。

**后台巡检**：`scheduler::run_login_state_scanner` 每 5 分钟执行一次 `refresh_due_credentials`——只有真正到期的账号才会发起请求，空闲时零流量。与打卡调度同一模式（`async fn` 由 Tauri runtime spawn，因为 `.setup()` 闭包没有 Tokio reactor）。

**GUI**：无法自愈的账号在行内标为琥珀色**「需重新绑定」**（独立 class，与状态 pill 区分开——"健不健康"和"要不要你动手"是两件事），tooltip 给出失败原因；每行新增**「重新绑定」**按钮（`wb_relogin`），粘贴新登录态或用扫码覆盖该条记录并保留 label / 积分 / 启用状态 / 默认身份。最初还有一个「🔁 全量刷新登录态」按钮（`wb_refresh_logins`），**已在 2.0.3 删除**，理由见第 13 节。

**测试**：`needs_refresh` 兜底窗口（含"已知过期优先于兜底"）、`credential_id_for` 的 token 无关性与 enterpriseId 区分、`parse_login_state` 稳定 id、导入不改写已刷新账号且保留默认身份、合并识别旧 token 哈希条目；集成测试新增完整生命周期（登录 → 刷新换 token → 导出导入 → 重新登录）断言**始终只有一行且默认身份不变**。

---

## 7. 安全与风险

| 风险 | 处置 |
|---|---|
| 导出文件是"可直接使用的凭据集"，等同密码本 | 文件 0600；UI 文案明确警示；加密为可选项而非默认（默认路径零遗忘口令风险）；文档写明"只在自己的设备间传递、用完即删" |
| 口令太弱 → 离线爆破 | Argon2id 19 MiB 内存成本；文档建议使用长口令；不提供"记住口令"（口令不入盘、不入配置） |
| token 泄漏进日志 / UI / 错误串 | 日志与预览只出现短 id 与计数；错误信息不回显 token；`ImportPreview` 结构体本身不含 token |
| 手改包导致 id 与内容不一致 | 导入时对每条校验 `credential_id(access_token) == id`：不一致则**重算 id** 并记 warning（而不是拒绝整包） |
| 替换模式误操作 | 二次确认 + 写盘前自动备份三个文件 |
| 旧版本读到新版本包 | `version` 更高时继续解析并 warning（能读懂的部分照常导入，比整包拒绝有用） |
| 导出目录不可写 / 无下载目录 | 回退到数据目录 `~/.proxy-rs/`，并把实际路径回显给用户 |
| 破坏 `PROXY_DATA_DIR` 隔离约定 | 导入命中路径全部由 `data_dir()` 派生，不新增任何绝对路径状态 |

---

## 8. 测试计划

**`src/pool_transfer.rs` 单测**（17 个，全部通过）

1. 明文往返：导出 → 导入，账号/密钥/默认身份/打卡设置逐字段相等。
2. 导出归一化：`cooldown_until_ms` / `last_error` 被清空，`machine_id` / `last_checkin_date` 保留。
3. 加密往返：同一口令 → 与明文包导入结果逐字段一致；错口令 → 明确报错且**不写盘**（断言池仍为空）。
4. 加密包外层不含明文：断言序列化文本中不出现 `access_token` 片段、密钥片段与域名。
5. 合并策略：新增 / 覆盖 / 未变三种计数；包内缺 `refresh_token` 时保留本机的更新值。
6. 替换策略：本机多余账号被移除，两处原文件都生成 `.proxy-rs-backup-*`。
7. 悬空默认身份：包内默认 id 不存在 → 清空 + 警告（且警告里点名那个 id）。
8. id 不一致：`access_token` 被改过 → 重算 id + 警告，导入后 id 与 token 一致。
9. 畸形输入：非 JSON、`format` 不符、`kind` 未知 → 各自的明确错误。
10. `version` 过高、checksum 不匹配 → 有警告但仍导入。
11. 超大文本（> 1 MiB）→ 解析前就被拒绝。
12. 文件名清洗：`../..` 路径被剥离、扩展名补齐、中文名可用、空名回退默认。
13. 0600 权限：导出文件与两个含密钥的存储文件都是 `0600`。
14. 导出文件名日期：UTC+8 与 UTC−5 各自落在正确的本地日期（覆盖负偏移不被 clamp 成 0）。

测试用共享的 `settings::data_dir_test_lock()` 串行化 `PROXY_DATA_DIR`，并把目录指向 `std::env::temp_dir()` 下的独立路径，不触碰真实 `~/.proxy-rs`。

**集成测试**：`tests/pool_transfer_roundtrip.rs`——在临时数据目录里构造两个登录态 + 两个密钥 + 一个**非首位**默认身份 → 导出 → 换一个空数据目录 → 导入 → 断言 `active_api_key()` / `effective_default_identity()` / `ordered_credentials()` / 密钥 id 列表全部回到导出前；并额外断言导入后的账号仍能产出 `X-User-Id` 等指纹头。加密路径同样跑一遍，且先验证错口令后池仍为空。

**前端**：`ui/tools/smoke.mjs` 的 `MOCK` 增加 `pool_export` / `pool_import_preview` / `pool_import` 三个桩，并在原有两条集成断言之外新增第三条（账号池传输）：导出弹窗写入口令后断言导出文案与**口令确实透传**；导入弹窗断言预览渲染出的新增/更新计数、警告文案与默认身份，且**预览成功前 `确认导入` 是禁用的**；点确认后断言触发了 `pool_import`、身份池被重绘、弹窗关闭。`task ui-check` 保持全绿（新 id 有 `wb-` 前缀且只创建一次；两个弹窗为按需创建，属于 `--runtime` 允许的 "built on first use"）。

**门禁**：`task check` 全绿（fmt / clippy `-D warnings` / test / ui-check / ui-smoke）。

---

## 9. 实施顺序与里程碑

| 阶段 | 内容 | 依赖 |
|---|---|---|
| M1 | `src/pool_transfer.rs`：格式模型 + 归一化 + checksum + 单测（明文路径先行） | 无 |
| M2 | 加密信封（argon2 + aes-gcm + base64 依赖）+ 解密/错口令测试 | M1 |
| M3 | 合并/替换 + 预览 + 备份原子落盘 + 集成测试 | M1 |
| M4 | Tauri 命令 `pool_export` / `pool_import_preview` / `pool_import` / `reveal_path` + 日志 | M2、M3 |
| M5 | GUI：`pool-transfer.js` + `form.js` 按钮 + `workbuddy.css` + `index.js` 装配 | M4 |
| M6 | `smoke.mjs` 桩与断言、README（数据目录表 / 新功能说明）、版本号 1.9.13 → 2.0.0（`Cargo.toml` / `src-tauri/Cargo.toml` / `package.json` / `tauri.conf.json`）、`task check` 全绿 | M5 |

---

## 10. 验收清单

- [ ] 导出：GUI 点「导出账号池」弹出系统保存面板，选定后得到文件，权限 0600，内容可被 `python -m json.tool` 解析，`cooldown_until_ms`/`last_error` 为空
- [ ] 导出（取消）：在保存面板里直接关闭 → 不写任何文件、UI 提示「已取消导出」且不是错误态
- [ ] 导出（加密）：同一界面勾选口令后，文件外层不含任何 token 片段；用错口令导入报「口令错误或文件已损坏」
- [ ] 导入（合并）：在另一数据目录导入后，账号与密钥数量正确，默认身份徽章与导出机一致
- [ ] 导入（替换）：本机原有账号被移除，**已存在的**状态文件各自生成一份 `.proxy-rs-backup-*` 备份，可手工回滚
- [ ] 导入立即可用：无需重启应用，请求日志出现 `key=<导入的 id>`，`/v1/messages` 流式对话成功
- [ ] 悬空默认身份：手工把包内 `default_identity_id` 改成不存在的 id → 导入成功、默认身份被清空、预览显示警告
- [ ] 篡改 `access_token`：导入时重算 id 并给出警告，不产生 id 与内容不一致的记录
- [ ] 安全：整个导出/导入过程中 `~/.proxy-rs/logs/proxy.log` 与 GUI 日志**不出现**任何完整 token
- [ ] 隔离：`PROXY_DATA_DIR=~/.proxy-rs-dev` 时导入只影响 dev 目录，`~/.proxy-rs` 保持不变
- [ ] `task check` 全绿；未使用新功能时行为与 1.9.13 完全一致

---

## 11. 实现状态与取舍记录（2.0.0 / 2.0.1 落地后回填）

规划在实现中被验证，也有几处明确的取舍，记在这里以免下一版重复讨论：

1. **校验和不匹配只警告不拒绝**，但**内容会保留**。用户手工改过包是可能的（改 label、删一个账号）；直接拒绝会让文件不可用，而静默接受会隐藏"有人动过这个文件"这一事实——而后者恰恰是排查导入结果异常时最需要的信息。
2. **id 与内容不一致时重算而非报错**。真正需要防的是"两个不同 token 塌缩到同一个 id 后其中一个消失"，重算 + 警告同时解决了可用性与正确性。~~规则是 `credential_id(access_token)`~~ —— **这条规则本身在 2.0.1 被推翻**：token 每次刷新都会变，用它的哈希当身份会让已刷新的账号"对不上自己"，进而造成重复账号与默认身份丢失。现改为按 `account.uid` 派生，详见 6.6 节。
3. **初版不引入 `tauri-plugin-dialog`**（导出目录固定、只能改名）。这一点已在 6.3 节被推翻：导出改用原生「另存为」面板，**仅 Rust 使用、不授予前端 `dialog:` 权限、不使用会 panic 的 `blocking_save_file`**。原判断只看到了"省一个插件"的收益，低估了固定目录在新机器上的不便。
4. **云端同步（COS + 数字密码）搁置**，理由与已确认的后续决定见 6.4 节：没有服务端就无法做到"只输数字密码"而不把长期密钥分发到每台机器。

落地清单（对应第 9 节里程碑）：

| 阶段 | 交付物 | 状态 |
|---|---|---|
| M1–M3 | `src/pool_transfer.rs`（格式、归一化、checksum、Argon2id + AES-256-GCM、预览/合并/替换、备份原子写入） | ✅ v2.0.0 |
| M3 | `tests/pool_transfer_roundtrip.rs`：明文与加密两条路径都断言"新机器上 `active_api_key` / `effective_default_identity` / `ordered_credentials` 回到导出前" | ✅ v2.0.0 |
| M4 | `pool_export` / `pool_import_preview` / `pool_import` / `reveal_path`；导入后 `reload_pool()` + 运行中则重启代理（无需重启应用） | ✅ v2.0.0 |
| M5 | `ui/src/views/settings/pool-transfer.js` + 工具栏两个按钮 + `.wb-transfer-*` 样式（写入既有 `workbuddy.css`）+ `index.js` 装配 | ✅ v2.0.0 |
| M6 | `smoke.mjs` 传输断言、README 能力表、版本号 1.9.13 → 2.0.0 | ✅ v2.0.0 |
| M7 | 导出改用原生「另存为」面板：`tauri-plugin-dialog` + 回调式 `save_file` + 取消语义 + 前端文案/提示调整 | ✅ v2.0.0 |
| M8 | 「导出内容为空」不再冒充成功：`ExportOutcome::warnings` / `is_empty()` + `store_health()` 三态 + 弹窗显示数据目录与导出数量 | ✅ v2.0.0 |
| M9 | **账号稳定身份（uid）+ 启动迁移 + 登录态自愈**：`credential_id_for` / `migrate_credential_ids` / `upsert_credential` 按身份匹配、`last_refresh_at_ms` 兜底主动续期、后台巡检、`wb_relogin`、GUI 每行「重新绑定」+「需重新绑定」标记 | ✅ v2.0.1（按钮已在 2.0.3 移除）|

实现中还补了几处规划未写明的细节：

- **`settings::data_dir_test_lock()` 改为跨模块共享**：该锁原先藏在 `settings` 的 `mod tests` 里，而 `pool_transfer` / 集成测试同样要重定位 `PROXY_DATA_DIR`。三个模块各持一把私有锁等于没锁，测试会随机互相污染，因此提升为 `#[cfg(test)] pub(crate)`。
- **`pool_export` / `pool_import` / `pool_import_preview` 走 `spawn_blocking`**：整包读取、Argon2id 派生（约 19 MiB / 2 轮）、三文件写入，以及 6.3 节起新增的保存面板（等待用户）都是阻塞操作，直接在 async 命令里跑会占住 webview 其他命令共享的工作线程——这与仓库既有的 `get_stats` / `get_request_logs` 约定一致。
- **`pool_export` / `pool_import` 的日志只记计数与短 id**，且导出成功后立即 `drop(text)`，不让整份含 token 的字符串在日志作用域里多活一步。
- **`spawn_blocking` 的返回类型需显式标注**：闭包返回 `Result<Option<(PathBuf, ExportOutcome)>, anyhow::Error>`，用 `Ok::<_, anyhow::Error>(None)` 让 `None` 分支的类型推断成立（否则 `?` 无法定错）。

### 验收状态

第 10 节清单里，**已由自动化覆盖**的是：0600 权限、归一化、错口令不写盘、加密包无明文、备份文件生成、悬空默认身份、畸形/超大输入、`PROXY_DATA_DIR` 隔离、「新机器上确实可用」，以及 2.0.1 的**身份稳定性**（token 轮换不改 id、重新登录不新增重复、导入不改写已刷新账号且保留默认身份、兜底续期窗口）——这些落在 `src/pool_transfer.rs` 的 22 个单测、`src/workbuddy_auth.rs` 的若干单测、`tests/pool_transfer_roundtrip.rs` 的 3 个集成测试（含完整生命周期：登录 → 刷新换 token → 导出导入 → 重新登录）和 `ui/tools/smoke.mjs` 的传输/登录态恢复断言里。

2.0.1 的迁移逻辑额外用**真实数据的副本**在隔离目录跑过一遍：4 个账号全部重键、无重复、`default_identity_id` 正确跟随、token 未变、二次运行改动为 0。

**仍需人工验收**的是与真实面板/真实上游相关的四条：

- [ ] 点「导出账号池」弹出的是**真正的系统保存面板**，标题为「导出账号池」、预填 `proxy-rs-pool-日期.json`、起始目录为下载目录，且是吸附在窗口上的 sheet；
- [ ] 在面板里直接关掉 → UI 显示「已取消导出（未写入任何文件）」，无错误色，按钮可再次点击；
- [ ] 在另一台机器（或换 `PROXY_DATA_DIR`）导入后，跑一次 `/v1/messages` 流式对话，确认日志出现 `key=<导入的 id>`；
- [ ] 目视确认深色模式下的传输弹窗（提示条、预览卡片、警告列表）配色正常。

### 已知问题（与本次改动无关，但值得另行处理）

`cargo metadata` 显示 workspace 的 `workspace_default_members` **只含根 crate `proxy-rs`**，因此仓库根执行的 `cargo clippy --all-targets` / `cargo test`（也就是 CI 的 `.github/workflows/ci.yml`）**不会编译 `src-tauri`**。这意味着新增的 GUI 侧代码在 CI 上不会被检查——本地是靠显式 `cargo clippy -p proxy-rs-gui` 才发现的。修法是在根 `Cargo.toml` 的 `[workspace]` 里显式声明 `default-members = [".", "src-tauri"]`，或在 CI 中加一条 `-p proxy-rs-gui` 的命令；这会改变 CI 的构建范围与耗时，故未擅自改动。

---

## 12. 2.0.2：去重与 `proxy.rs` 拆分

两件事，都不改变行为。

### 12.1 消掉重复逻辑（`workbuddy_auth.rs`）

`refresh_all_credentials` 与 `refresh_due_credentials` 有 **75% 逐行重复**（58 行里 44 行相同）：整段 `refresh_credential` 的 match、`needs_relogin` 判定、`upsert_credential(marked)` 与计数全被抄了两遍，两者只差**候选集**。风险不在行数，而在分叉：以后改 `needs_relogin` 的语义（例如区分 401 与 403）很可能只改一处，而两处都在向用户展示"哪些账号需要重新绑定"。

现在抽出 `refresh_one_credential`（单账号结果）与 `refresh_candidates`（遍历 + 汇总），两个公开函数各自只负责选集合：50+61 行 → 7+8 行。

同时发现积分路径同样重复（`refresh_all_points` / `refresh_one_points` 25/27 行里 12 行重合），一并抽成 `refresh_one_credential_points` / `refresh_one_key_points`，并把出现三次的密钥显示名逻辑收成 `key_display_label`。

### 12.2 `proxy.rs` 按领域拆分

`proxy.rs` 是仓库最大的文件（3306 行生产代码 + 2212 行测试，合计 5518）。Rust 的编译单元是 **crate 而非文件**（实测：`touch` 任一文件都会重编整个 crate），所以拆分**不改进编译速度**；拆它的唯一理由是**内聚性**——一个文件里同时装着三个 flavor 的 handler、多上游 failover、SSE 组帧、凭据切换与错误分类，改 A 领域要读 B 领域。

拆成模块根 + 5 个领域文件：

| 文件 | 生产行数 | 职责 |
|---|---|---|
| `proxy.rs`（模块根） | 2199 | 三个协议 handler、`forward_request`、统计落库 |
| `proxy/auth_resolvers.rs` | 57 | 调用方身份（请求带的是哪个 key） |
| `proxy/auth_headers.rs` | 140 | 上游鉴权 + CLI 指纹头 |
| `proxy/failover.rs` | 191 | 何时离开默认身份 + failover 短记忆 |
| `proxy/upstream_errors.rs` | 279 | 错误分类、描述、降级重试 |
| `proxy/sse.rs` | 576 | SSE 组帧、请求日志台账、TTFT/速度 |
| `proxy/tests.rs` | 2142（测试） | 全部既有单测 |

**不改 API**：模块根 `pub(crate) use` 把拆分项按原名导出，因此既有的 `crate::proxy::X` 与测试里的 `super::X` 路径全部照旧解析。拆分是**文件布局变化**，不是接口变化。

**正确性验证**（这一步比拆分本身更重要）：按拆分时施加的两个机械变换（生产区加 `pub(crate)`/`pub(super)` 前缀；测试区去掉 `mod tests {` 带来的 4 空格缩进）逐区还原后，与拆分前的 `HEAD` 版本比对：

- `auth_resolvers` / `auth_headers` / `upstream_errors`：**逐字节相同**；
- `failover` / `sse`：仅差 2 个为测试可读而加的 `pub(crate)` 字段标记；
- `tests`：去掉空白与 rustfmt 合并行导致的尾逗号后**完全一致**；
- 测试数量 18 `#[test]` + 30 `#[tokio::test]` = **48**，拆分前后一致；`cargo test` 428 通过，与拆分前同数。

拆分过程中被编译器与 clippy 各拦下一次真实问题，值得记录：`blocking_save_file` 式的隐藏坑没有出现，但（a）`tests.rs` 起初仍包着 `mod tests { … }`，使它变成 `proxy::tests::tests`、`super::` 指向错层（44 个错误），（b）两个 `pub(crate)` 函数暴露了私有 `ApiFlavor`，clippy 直接拦下——后者正是"拆分时顺手放宽可见性"的典型代价，最终把这两个函数收窄为 `pub(super)`。

---

## 13. 2.0.3：删除「全量刷新登录态」按钮，改由徽章承担报告

### 为什么它冗余

2.0.1 加这个按钮时的理由是「401 后无从恢复」。但同期引入的自动路径已经把它的职责覆盖完了：

| 场景 | 现在由谁处理 |
|---|---|
| 令牌到期前主动续期 | 后台巡检（每 5 分钟，`refresh_due_credentials`）|
| 请求撞上 401 | 反应式刷新并重试同一凭据（`proxy.rs`）|
| 需要人工重登 | 每行「重新绑定」|

它只剩「**立即、对所有账号（含未到期的）**强制执行一次刷新」这一点自主行为，而未到期的账号本来就不该被刷——强制刷只是多花一次刷新往返。

另有一个容易误解的点：按钮的 `reload_pool()` + 重启代理**并非必需**。`refresh_credential` 只写磁盘；运行中的池由请求路径自己 `pool.update_credential()` 更新，所以不点按钮也不会用到旧令牌。

### 但删之前必须先补一个缺口

徽章原本的判据是纯静态的：

```rust
let needs_relogin = c.enabled && c.refresh_token.is_none();
```

它只抓「没有 refreshToken」。而**有 refreshToken 但被接口拒绝（401/403）**——同样只能靠重登恢复——静态判据抓不到，只有真刷新一次才知道。而这项「试一次并报告」正是按钮的第二个作用；巡检虽然也能发现，但它写的 `last_error` **从未在界面上渲染**（`grep last_error ui/src/views/settings/*.js` 无结果），所以发现只进了日志。

因此先补上判定与呈现，再删按钮：

- **新增持久字段** `relogin_required_since_ms`：与 `last_error` 分开，因为一次网络抖动和一次 refresh token 被吊销都会写后者，只有前者不可自愈，且必须跨重启保留（否则徽章会忘掉一个已永久搁浅的账号，用户要等请求失败才知道）。刷新成功或重新登录时清除。
- **判定收敛到一处** `refresh_failure_needs_relogin(cred, error)`：无 refreshToken、或错误含 401/403/refreshToken → 需要重登；连接失败/5xx/超时等可重试错误**不**标记（否则网络一抖全池亮灯，徽章就不再意味着什么）。
- **反应式路径也落盘**：原先 `refresh_workbuddy_credential` 只 `pool.mark_refresh_failed`（内存，60 秒即过期、进程结束即丢），现在同时写持久判定。
- **徽章 tooltip 说明原因**：区分「没有 refreshToken」与「刷新被拒」两种文案，并附上上游错误——把原先只存在于实时日志的信息放到行上。

### 删除范围

- 前端：工具栏按钮与其 41 行点击处理器；
- 后端：`wb_refresh_logins` 命令（33 行，含 `reload_pool` + 重启代理）及其 handler 注册；
- 库：`refresh_all_credentials`（唯一调用方即该命令）。

`refresh_one_credential` / `refresh_candidates` 保留——巡检仍在用。

### 验证

除单测（判定矩阵：401/403/无 token 需重登，连接失败/500/坏 JSON 不需；以及判定落盘且只在恢复后清除）与 smoke（新增「有 refreshToken 但被拒」的 fixture，断言该行仍被标记且 tooltip 含原因）外，**用真实上游跑通了整条链路**：在隔离数据目录种入一个 `last_refresh_at_ms` 已过 6 小时兜底窗口、refreshToken 无效的账号，启动应用后巡检自动触发，上游返回真实的 `401 code 12153 refresh token failed`，判定随即落盘（`relogin_required_since_ms` 有值），且**进程退出后仍在**——正是徽章需要跨重启保留的那份数据。
