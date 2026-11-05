# proxy-rs v3.0.0 发布说明：客户端访问账户（入站鉴权）+ Windows 支持

> 版本：**3.0.0**（上一版 2.4.1）
> 这是一个**大版本**：新增一条与既有凭据体系正交的入站鉴权通道，并首次产出 Windows 安装包。
> **`/v1/*` 的转发协议语义、上游鉴权头、`UPSTREAM_API_KEY_PASSTHROUGH` 全部未改。**

---

## 为什么是大版本

v2.4.1 之前，网关的鉴权只有一种方向：**出站** —— 网关拿什么身份去请求上游（静态 key / 密钥池 / WorkBuddy 登录态）。这条链路在公网部署时有根本缺口：

`/v1/*` 数据面**没有任何客户端鉴权**。谁都能访问你的网关，用你的上游账号额度。README 里对此的表述是诚实的，但只适用于「单机自用」。

v3.0.0 补上的是**入站**方向：谁可以调用这个网关。两套凭据从此**分开管理**。

---

## 核心设计：两把 key，两个方向，互不干扰

这是本版唯一需要认真理解的概念。用户明确要求「上游的 key 和网关下游的 key 分开管理」，方案据此设计：

| | 上游 key（**本版未改**） | 网关访问 key（**新增**） |
|---|---|---|
| 用途 | 客户端透传给上游做鉴权 | 客户端向**本网关**证明身份 |
| 携带位置 | `x-api-key` / `authorization: Bearer` | **专用头 `x-proxy-rs-key`** |
| 处理方 | `auth_resolvers.rs` → `auth_headers.rs` → 上游 | 入站中间件，校验后**剥离**，不参与转发 |
| 存储 | `api-keys.json` / `GuiSettings.api_key` | `client-accounts.json` |
| 开关 | `UPSTREAM_API_KEY_PASSTHROUGH` / 身份池 | `client_auth_enabled`（独立） |

一次请求可以同时带两个头：前者原样走上游，后者在门口被消费。**两者可以同时启用，没有任何互斥。**

### 为什么必须用独立的 header

如果复用 `x-api-key`，一个 header 会有两种语义，取决于哪个开关开着。后果是网关访问 key 会被当成上游密钥转发出去 —— 既是错误，也是凭据泄漏。用独立 header 从结构上排除了这件事，并且有测试钉住：

`tests/client_auth.rs::both_keys_travel_their_own_directions` 同时开启 passthrough 与入站鉴权，断言：
1. 上游收到的 `x-api-key` 仍是客户端原值；
2. 上游收到的 `authorization` 仍是 `Bearer <客户端上游 key>`；
3. `x-proxy-rs-key` **不出现在**任何出站头里，其值也不出现在任何头的值中。

---

## 新增能力

### 1. 入站访问账户（`x-proxy-rs-key`）

- **只存 SHA-256 哈希**，不存明文。明文在创建时返回**唯一一次**，之后不可恢复，只能吊销重发。
- 账户 id 由哈希派生（`ca-<6 hex>`），因此**导出/导入在没有明文的情况下**也能校验并自愈。
- **默认关闭**。这是硬性要求：升级后不配任何东西，现有 `ANTHROPIC_BASE_URL=http://localhost:3456 claude` 必须继续能跑。
- 开关开启但账户表为空 → **一律 401**，绝不 fail-open。
- 改动**热生效**：GUI 或 HTTP 禁用一个账户，下一个请求即 401，无需重启。

**401 的响应体是 Anthropic 风格**，不是项目通用的 `proxy_error`：

```json
{"type":"error","error":{"type":"authentication_error","message":"...","code":"invalid_key"}}
```

理由：Claude Code / Codex 会把 `proxy_error` 当网关故障并**重试**，对一个错误的 key 意味着无限重试。`authentication_error` 才是它们停下来提示用户修正配置的信号。`code` 区分 `missing_key` / `invalid_key` / `account_disabled` —— 一个被禁用的账户和一个拼错的 key 在 401 体里看起来一样，但排查方向完全不同。

### 2. 两个管理入口

**GUI**：设置页新增「访问账户（客户端接入）」区块，位于身份池之后，带醒目提示说明方向相反。创建后弹窗一次性展示完整 key 与 `x-proxy-rs-key: <key>` 请求头示例。

**HTTP**（供 headless / 脚本）：

| 方法 | 路径 | 说明 |
|---|---|---|
| GET | `/api/admin/client-accounts` | 列表（脱敏，不含 `key_hash`） |
| POST | `/api/admin/client-accounts` | 创建，**唯一一次**返回明文 `api_key` |
| PATCH | `/api/admin/client-accounts/{id}` | 改备注 / 启禁用 |
| DELETE | `/api/admin/client-accounts/{id}` | 删除 |

全部需要 `Authorization: Bearer $PROXY_API_TOKEN`，且**拒绝任何带 `Origin` 的请求** —— CORS 是 `allow_origin(Any)`，浏览器可以「发出」请求，所以 Origin 检查才是真正起作用的那道闸。这样「你随便打开的一个网页能给自己签发网关密钥」变成「不能」。

### 3. 访问账户随账号池一起导出

用户要求「账户/key 要和账号池导出功能一起导出」。导出格式升到 **v2**：

- `client_accounts` 作为第四块数据，与账号池/密钥池/偏好并列；
- **双向兼容**：v1 旧包导入不报错且不丢账号（新字段 `serde(default)`）；旧版读 v2 走既有的「警告不拒绝」路径；
- 加密包里这一块与其他三块一样被清出 envelope，只留 counts；
- **迁移后客户端原 key 照常可用** —— 导出的是哈希，校验本来就比哈希。

### 4. Windows 支持

- `src/util.rs` 的 Windows 时区编译错误已修：`libc::localtime_r`/`gmtime_r` 在 Windows 不存在，改用 `localtime_s`/`gmtime_s`。**不是简单改名** —— 这两组函数参数顺序相反（出参在前）、返回值是 `errno_t`（0 为成功）而非指针。
- 交叉编译链已实测打通：`cargo-xwin` + `+crt-static` 产出**零依赖单 exe**（仅依赖 `KERNEL32`/`ntdll`，无需 VC++ 运行库）。
- CI 新增 `windows-latest` leg，产出 `.msi` + `.exe`（NSIS）。两个平台统一显式指定 `--bundles`，不依赖 `tauri.conf.json` 的默认值。

---

## 观测性

三个转发 handler 的 INFO 日志行追加 `client_account=<label>`（门禁关闭时为空串，保持行格式统一）。`x-proxy-rs-key` 已加入 `SENSITIVE_HEADERS` 脱敏名单 —— 它不含既有的 `api-key` 等片段，不补就会明文落 `proxy.log`。

---

## 兼容性

| 场景 | 行为 |
|---|---|
| 升级但不开启入站鉴权 | **无感知**。所有既有 CLI 配置原样可用 |
| `UPSTREAM_API_KEY_PASSTHROUGH=true` + 入站鉴权同开 | **支持**，两把 key 各走各的头 |
| 导入 v1 旧导出包 | 正常导入，账户表为空，无报错 |
| 旧版读 v3.0.0 的导出包 | 警告「可能丢失部分内容」，不拒绝 |
| 既有身份池 / 登录态 / 定时循环 | 未改 |

---

## 测试

| 层 | 数量 |
|---|---|
| `cargo test --lib` | 589 项（含 `client_accounts` 9 项、`client_auth` 4 项、导出 v2 新增 8 项） |
| 集成测试 | 47 项（含 `tests/client_auth.rs` 9 项、`tests/client_auth_admin.rs` 7 项） |
| UI 静态 + 运行时 + 冒烟 | 全绿（`check-ui` / `check-ui --runtime` / `smoke.mjs`） |
| `cargo clippy -D warnings` | 干净 |
| `cargo fmt --check` | 干净 |
| Windows 交叉编译 | `cargo xwin check --lib` 通过 |

关键回归用例（`tests/client_auth.rs`）：

- 门禁关闭时无 key → 不被拒（升级安全）
- 门禁开启无 key / 错 key / 禁用账户 → 401，且 `code` 正确
- 开启但账户表为空 → 401（fail closed）
- `/health` 与 `/api/*` 不受门禁影响（反代探活 / 桌面 GUI 的本机 fetch）
- 禁用账户后**下一个请求**即生效
- **两把 key 各走各的方向**（见上文）

---

## 已知边界

- **客户端能否附自定义请求头需逐个实测**。`curl` 与多数 SDK 可以直接带 `x-proxy-rs-key`；Claude Code / Codex / DSH 的自定义头支持尚在验证。不支持时的兜底是**在反代层按可信来源注入**，而不是回头去复用 `x-api-key`。
- 公网部署时，`/api/*` 与 `/metrics` 应由反代 deny —— 网关层刻意没有给它们加门禁，因为桌面 GUI 靠本机 `fetch` 读 `/api/*`，加在这里会把它自己打断。
- 本版不做每账户配额 / 限流 / 计费统计。`last_used_ms` 仅用于展示「谁在用」。
