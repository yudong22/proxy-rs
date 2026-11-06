# proxy-rs v3.1.0 发布说明：纯服务器模式 + Web 管理台

> 版本：**3.1.0**（上一版 3.0.2）
> 两个新能力：**headless 服务器二进制**与**浏览器管理台**。桌面端行为不变。

---

## 1. 纯服务模式：`proxy-rs-server`

新增 `src/bin/proxy-rs-server.rs`。同一个 `build_app_router`，**不含 Tauri**，产物是单个可执行文件（release 约 6.7 MB）。

桌面版依赖图形会话：窗口、托盘、登录会话结束即退出。服务器上这些都不存在，且桌面版会随会话注销被杀。`src/router.rs` 的注释早就写着它「kept free of GUI types so a headless front-end can reuse it unchanged」，这个二进制就是那个 front-end。

```bash
cargo build --release --bin proxy-rs-server
```

与桌面版**共用**全部配置来源（`gui-settings.json` + `.env` + `PROXY_DATA_DIR`）、凭据池、`stats.db` 与三个后台循环，所以从桌面端迁来的安装不丢状态。

### 与桌面版的三条语义差异

| 项 | 桌面版 | 服务器版 | 理由 |
|---|---|---|---|
| 启停/托盘 | GUI 控制 | **无**，归 systemd | 进程生命周期不属于应用 |
| 入站鉴权开关 | 默认关 | **必须显式 `PROXY_CLIENT_AUTH=true`** | 可达即需鉴权；启动日志会打印实际值，关闭时另有 WARN |
| `/api/*` `/metrics` `/health` | 本机 dashboard 读 | 反代层 404 | 只对回环有意义 |

### 部署

systemd 单元、nginx 反代、证书、数据迁移、验收清单与踩坑记录见 [`deploy/linux/README.md`](../deploy/linux/README.md)。

---

## 2. Web 管理台：直接复用桌面版 UI

服务器版带一个浏览器控制台，地址 **`https://<域名>/admin/`**。

**它不是另写的一套界面。** 网关提供 `ui/` 里的**同一个** `index.html`、同一批模块、同一批样式表——浏览器里跑的就是桌面版那个控制台，布局、文案、空态、快捷键、交互完全一致。唯一差别在传输层：

```
桌面版：invoke('get_status')  →  Tauri IPC
服务器：invoke('get_status')  →  POST /admin/api/rpc/get_status
```

`ui/src/core/ipc.js` 本来就把所有调用收敛到一个 `invoke()`，所以服务器只需在页面加载前用 `ui/server/bridge.js` 填上 `window.__TAURI__`。**`ui/src/**` 一行没改。**

### 为什么这么做

第一版服务器控制台是单独写的——那是个错误，而且不是审美问题。它在一个版本内就和桌面版长得不一样了，而且修了一处不会带到另一处。**两份前端就是两个要同步的产品**，而没人要求第二个。

### 登录

独立的登录页，用 `PROXY_API_TOKEN` 换会话 Cookie：

- `HttpOnly` —— XSS 偷不走会话
- `SameSite=Strict` —— 跨站请求根本不带它，CSRF 在源头消失
- `Path=/admin` —— 不发给 `/v1/*`
- 12 小时滑动续期（可勾「记住我」延到 7 天），进程内存保存，重启即失效
- **登录限速**：同一地址连续 5 次失败锁 1 分钟，重复犯翻倍至 1 小时封顶。按地址而非全局计数 —— 全局计数会让陌生人把运维锁在自己网关外面
- 失败但未锁定时，页头提示「再失败 N 次将锁定」，**在锁定前**给出可行动的警告
- 锁定期内按钮进入倒计时并保持禁用，而不是让用户对着一个点不动的按钮猜

### 修复：无限重启

第一版有个死循环：`serve_page` 只检查 cookie **是否存在**，不校验是否有效。于是过期 cookie 会拿到控制台页面 → 页面上每个 RPC 都 401 → 桥接 `location.reload()` → cookie 还在 → 又是控制台页面 → 无限转。

两处都修了：

- 服务端**校验**会话（`touch`，同时滑动续期），无效就直给登录页；
- 桥接遇 401 跳 `/admin/login`（`location.replace`，不加历史），并加去重标志 —— 启动瞬间会并发多个 RPC，否则一次过期触发多次跳转。

### 刻意做不到的三件事

| 能力 | 处理 | 原因 |
|---|---|---|
| 启停服务 | 点了提示「由 systemd 托管，请用 systemctl」 | 进程生命周期不归应用 |
| 打开日志目录 | 静默成功（只用来关 toast） | 服务器没有文件管理器 |
| 扫码登录 | 明确拒绝并说明 | 浏览器回调只能指向本机 localhost |

**不假装成功**：按钮会告诉你原因，而不是点了没反应。

### 桌面端不受影响

桌面版仍走 Tauri IPC，`build_app_router` 收到的 admin router 是 `None`，**不暴露任何 HTTP 管理面**。

## 3. 一键写入客户端配置：现在真的能用

「写入 Claude Code / Codex / DSH 配置」这三个按钮此前都会写一个占位符 key（`ANTHROPIC_AUTH_TOKEN = "nouse"`、`PROXY_RS_API_KEY: nouse`）。桌面端恰好能用——网关默认不校验入站 key——但服务器上 `PROXY_CLIENT_AUTH=true`，写出来的配置**一调就 401**，用户只能重新签发一把再手工粘到三个文件里。

现在：

| 客户端 | 写入内容 |
|---|---|
| Claude Code | `ANTHROPIC_BASE_URL` + **真实可用的** `ANTHROPIC_AUTH_TOKEN` |
| Codex | provider 块（`base_url` / `wire_api` / `env_key`）+ `~/.codex/.env` 里的真实 key |
| DSH | patch 里的 provider + 凭据文件里的真实 key |

Codex 此前**只写了模型目录、没写 provider**，所以即便有 key 也连不上；这次一并补上。

### 代价：明文 key 落盘

要实现「写入可用的 key」，就必须能取回明文，于是 `client-accounts.json` 现在同时保存哈希和明文（`0600`）。这推翻了最初「只存哈希」的设计，因此补了三道约束：

- 字段是 `Option`，旧账号没有它仍能正常鉴权；
- **未加密的导出包会剥离明文**——否则搬账号池的文件就成了一份可用 key 清单；
- 鉴权仍然只比哈希，明文仅用于写配置。

## 4. 修掉的三个 bug

### Bug 1：`client_account=` 从来没被记录过（3.0.0 起一直存在）

请求日志里本应标注是哪个访问 key 发的请求，实际永远是空。

根因：网关把认证结果这样塞进请求：

```rust
request.extensions_mut().insert(identity);   // identity: VerifiedAccount
```

而 `Extensions` 是按**值的具体类型**存取的，不做 `Into` 转换。于是 map 里躺着的是 `Extension<VerifiedAccount>`，而每个 handler 取的是 `Extension<ClientAccountIdentity>` —— 两个类型永远对不上。请求照常认证、转发、返回 200，只是 `account_label()` 恒返回 `""`。

修复：显式 `ClientAccountIdentity::from(identity)`。

**为什么一直没被发现**：现有测试都只断言「通过/拒绝」，没有一个断言身份**传到了 handler**。为此新增了一条回归测试，它断言日志里出现 `client_account=<label>`，并且在还原 bug 后确实会失败。

### Bug 2：`PROXY_CLIENT_AUTH=true` 时管理台 401（只在真实部署上暴露）

入站访问密钥门禁（`client_auth::guard`）把**所有**非白名单路径都拦了，包括 `/admin/*`。而 `PROXY_CLIENT_AUTH=true` 恰恰是服务器部署必须设的值 —— 于是管理台在它唯一该存在的配置下彻底不可用，登录接口返回 `401 missing_key`。

修复：把 `/admin/*` 加入豁免。两者是**无关的凭据**：访问 key 是客户端调 `/v1/*` 用的，管理台令牌是人开浏览器用的。把管理台挡在访问 key 后面，等于要求每个浏览器用户手持一个能花上游额度的凭据。

**为什么本地测试没抓到**：单元测试要么把门禁关着，要么根本不碰 `/admin/*`。新增回归测试同时钉住两侧 —— `/admin/*` 不被拦、`/v1/*` 仍然被拦。

### Bug 3：`scheduler::snapshot` 报告错误的宿主进程

`process` 字段硬编码 `proxy-rs-gui`，`note` 写着「跑在桌面进程里：退出应用即停止」。服务器部署下这句话是错的，它让读 `last_run` 的人去找一个不存在的按钮。

修复：按 `current_exe()` 文件名判定，两种形态各自说自己的话。

---

## 5. nginx 反代要点

```caddyfile
/v1/* 等数据面  → 网关:3456    # proxy_buffering off 是 SSE 的关键
/admin/         → 网关:3456    # 两条 location，见下
/api/* /metrics /health → 404  # 无鉴权的只读观测面
```

**`/admin/api/` 必须单独一条 `location`**，不能和 `/admin/` 合并成一条 rewrite。控制台页面里的 API 基址是绝对的 `/admin/api`，如果只写 `^/admin/(.*)$ → /admin/api/$1`，页面发出的 `/admin/api/session` 会被再前缀一次变成 `/admin/api/api/session`，登录后整个控制台 404。nginx 选最长前缀匹配，所以拆成两条即可。

---

## 6. 验收（2026-11-06 实测，api.wuba.site）

| 项 | 预期 | 实测 |
|---|---|---|
| `cargo test` | 全绿 | ✅ 681 passed, 0 failed |
| `cargo clippy -D warnings` | 无告警 | ✅ |
| `cargo fmt --check` | 无差异 | ✅ |
| `ui/tools/check-ui.mjs` | 无问题 | ✅ 30 模块 / 10 样式 |
| 公网 `/admin` | 301 → `/admin/` | ✅ |
| 公网 `/admin/` | 200 控制台 | ✅ |
| 浏览器登录 → 六个标签页 | 全部渲染 | ✅ `admin-smoke` 全通过 |
| 浏览器存储的会话 Cookie | HttpOnly + Path=/admin | ✅ |
| 登出后 `/admin/api/session` | 401 | ✅ |
| `PROXY_CLIENT_AUTH=true` 下控制台 | 可访问 | ✅ |
| 数据面 `/v1/models` 带 key / 无 key | 200 / 401 | ✅ |
| `/api/*`、`/metrics` 公网 | 404 | ✅ |
| SSE 流式 | 逐帧到达 | ✅ |
| `kill -9` 后自愈 | 6 s 内 | ✅ |

---

## 7. 升级

### 从 3.0.x 升到 3.1.0（服务器）

```bash
cargo build --release --bin proxy-rs-server
systemctl stop proxy-rs
install -m 755 target/release/proxy-rs-server /opt/proxy-rs/bin/
systemctl start proxy-rs
systemctl status proxy-rs --no-pager
```

数据目录无需迁移。**若之前把 `/api/*` 或 `/admin` 写进了反代 404 规则**，按 §4 调整后 `nginx -t && systemctl reload nginx`。

### 桌面版

行为没有变化，按常规 `.dmg` / `.msi` 升级即可。新增的 HTTP 管理面在桌面版**不启用**。
