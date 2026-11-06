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

## 2. Web 管理台

服务器版带一个浏览器控制台，地址 **`https://<域名>/admin/`**：

```
浏览器 ──HTTPS──► nginx ──► 网关 /admin/api/*（自带会话鉴权）
```

### 能做什么

运行状态与今日统计、身份池管理（登录态 / API Key / 默认身份 / 打乱顺序 / 打卡与成长任务）、访问密钥增删、设置、日志、账号池导入导出、三个定时循环的状态。

### 鉴权：令牌换 Cookie 会话

用已有的 `PROXY_API_TOKEN` 换一个会话 Cookie：

- **`HttpOnly`** —— 管理台的 XSS 偷不走会话；
- **`SameSite=Strict`** —— 跨站请求根本不带它，CSRF 在源头消失，不需要额外的 CSRF 令牌；
- **`Path=/admin`** —— 不发给 `/v1/*`；
- **`Secure`** —— 由 `X-Forwarded-Proto` 判定，纯 HTTP 调试时不设（否则浏览器拒收，表现为登录死循环）；
- **12 小时滑动续期**，进程内存保存，重启即失效。

用同一个令牌而不是新造一个秘密：部署者已经必须设置它，两个秘密只会被设成同一个值。

### 刻意不做的事

| 桌面能力 | 服务器版 | 原因 |
|---|---|---|
| 启停服务 / 开机自启 | 无 | 归 systemd |
| 打开日志目录 | 无 | 没有文件管理器 |
| 「另存为」导出 | 改 HTTP 下载 | 浏览器自己决定存哪，服务端不必有写任意路径的能力 |
| 扫码登录 | 不做 | 需要浏览器回跳 `localhost`，服务器上没有 |

少一个按钮好过一个点了没反应的按钮。

### 桌面端保持不变

桌面版仍然走 Tauri IPC，入口传 `None`，**不暴露任何 HTTP 管理面**。多一个浏览器可达的入口就是多一处要鉴权的东西。

接口契约见 [`docs/admin-api.md`](../docs/admin-api.md)。

---

## 3. 修掉的三个 bug

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

## 4. nginx 反代要点

```caddyfile
/v1/* 等数据面  → 网关:3456    # proxy_buffering off 是 SSE 的关键
/admin/         → 网关:3456    # 两条 location，见下
/api/* /metrics /health → 404  # 无鉴权的只读观测面
```

**`/admin/api/` 必须单独一条 `location`**，不能和 `/admin/` 合并成一条 rewrite。控制台页面里的 API 基址是绝对的 `/admin/api`，如果只写 `^/admin/(.*)$ → /admin/api/$1`，页面发出的 `/admin/api/session` 会被再前缀一次变成 `/admin/api/api/session`，登录后整个控制台 404。nginx 选最长前缀匹配，所以拆成两条即可。

---

## 5. 验收（2026-11-06 实测，api.wuba.site）

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

## 6. 升级

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
