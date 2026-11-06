# 服务器模式管理 API（`/admin/api/*`）

> 版本：**3.1.0** 起提供。仅供 **headless 服务器部署**（`proxy-rs-server`）使用；
> 桌面版的等价能力走 Tauri IPC（`#[tauri::command]`），两者互不影响。
>
> **管理台就是桌面版那份 UI。** `/admin/` 提供的是 `ui/` 里的同一个前端，传输层由
> [`ui/server/bridge.js`](../ui/server/bridge.js) 把 Tauri IPC 换成下面的 `/admin/api/rpc/*`。
> 因此这里有两套接口：
>
> | 套 | 用途 | 谁在用 |
> |---|---|---|
> | `/admin/api/rpc/<command>` | **命令式**，命令名与参数形状**完全等于 Tauri 命令** | 桌面版控制台（经桥接） |
> | `/admin/api/<resource>` | REST 式，脚本与外部工具 | curl / 自动化 |
>
> 两者共用同一份实现（`src/admin.rs`），不会出现「REST 能改、控制台不能改」这种漂移。

---

## 为什么有这套接口

桌面版的控制台通过 Tauri IPC 直接调用进程内的 Rust 函数。服务器没有 webview，
也没有 `#[tauri::command]`，所以同一批能力必须以 HTTP 暴露——否则在一个没有图形
界面的 Linux 机器上，账号池、访问密钥、配置这些**只有 GUI 才能改**的东西就永远
改不了，只能手工编辑 JSON 文件。

## 与 `/api/*` 的关系（勿混淆）

| 命名空间 | 性质 | 鉴权 | 对外 |
|---|---|---|---|
| `/v1/*` | 数据面，客户端协议 | 访问 key（`x-proxy-rs-key` 等） | **是** |
| `/api/*` | 只读观测，+ 一条手动触发 | 读：无；写：`PROXY_API_TOKEN` | **否**（反代 404） |
| `/admin/api/*` | **管理面**，可改凭据/删账号 | 会话 Cookie（登录后） | **是**，仅当部署者主动在反代放行 |

`/admin/api/*` 刻意**不**复用 `/api/*`：后者被设计成「只读、无鉴权、绑回环、
反代一律 404」，把会改账号状态的写操作塞进去会破坏那个前提。两者是不同的信任
域，混用会让「反代已经把 `/api/*` 挡掉了」这条安全边界失效。

---

## 鉴权：令牌换 Cookie 会话

管理台是给人用的网页，不能用「每个请求都手动带 Bearer」那套。流程：

```
1. POST /admin/api/login   {token}
     → Set-Cookie: proxy_rs_admin=<session-id>; HttpOnly; SameSite=Strict; Path=/admin
     → Cookie 内存中保存，重启即失效（见下）
2. 之后所有 /admin/api/* 请求靠 Cookie 鉴权
3. POST /admin/api/logout  → 清除会话
```

### 令牌是什么

就是已有的 **`PROXY_API_TOKEN`**（`/etc/proxy-rs.env` 里那个）。不新增一个秘密：
部署者已经必须设置它才能用 `/api/*` 的写路由与账号管理，再引入第二个令牌只会
让人把它们设成同一个值。

未配置 `PROXY_API_TOKEN` 时，管理台的登录接口返回 `503 admin_disabled`，
页面显示如何配置而不是一个永远失败的登录框。

### 会话存哪

**进程内存**，不落盘：

- 重启网关即全部失效，符合「改完配置重启」的部署模型；
- 不需要给会话文件做权限与轮换，少一个可能泄漏的东西；
- 多实例部署本来就不共享状态，落盘反而制造「A 机会话在 B 机有效」的错觉。

会话 id 是 32 字节 CSPRNG，比较用常量时间。默认有效期 **12 小时**，
且**每次请求滑动续期**——一个正在用的管理台不该在改配置改到一半时突然登出。

### 登录限速

按**客户端地址**计数的失败预算，不是全局计数：

| 项 | 值 | 理由 |
|---|---|---|
| 失败上限 | 连续 5 次 | 够容纳两次手滑（令牌常从文件复制，尾随换行是经典坑），又少到无法猜测 |
| 首次锁定时长 | 60 秒 | — |
| 重复犯 | 翻倍至 1 小时封顶 | 一直锤的客户端每次多等一会儿 |
| 计数键 | `X-Forwarded-For` 首跳（无则 `X-Real-IP`） | 反代后面 socket 地址恒为 127.0.0.1，用它会让所有调用者共用一个桶 |

**为什么按地址而不全局**：全局计数会让任意陌生人把运维锁在自己网关外面 —— 把防爆破变成 DoS 原语。

响应带 `429` + `Retry-After`，body 里另有 `retry_after_secs`，登录页据此做倒计时。**锁定期内即使令牌正确也返回 429**：此时说「令牌错误」会让运维去找一个不存在的错字，而真实答案是「等一会儿」。

失败但仍未锁定时，响应头 `x-login-attempts-remaining` 给出剩余次数，登录页据此**在锁定前**提示。锁定后才提示是没用的 —— 那时已经无法改变结果了。

### 访问密钥的明文

> **行为变更**：访问账户现在会把 key 的明文一并保存在 `client-accounts.json`（`0600`）。

原先只存 SHA-256，代价是「一键写入客户端配置」只能写占位符 `nouse` —— 在没有 `PROXY_CLIENT_AUTH` 的桌面端刚好够用（网关不校验），但在服务器上（`PROXY_CLIENT_AUTH=true`）写出来的配置**根本用不了**，用户只能重新签发一把再手工粘贴三份。

补偿措施：

| 措施 | 说明 |
|---|---|
| 文件权限 | `0600`，与其余凭据文件一致 |
| 字段可选 | `key_plaintext` 是 `Option`；旧账号没有，仍能正常鉴权 |
| **未加密导出会剥离** | `pool_transfer` 在明文导出时清空该字段 —— 否则「把账号池搬到另一台机器」的文件就成了一份可用 key 清单 |
| 鉴权仍比哈希 | 明文只用于写进客户端配置，不参与验证 |

没有明文的账号可以**重新签发**（`reissue_key`）拿到一把新的；id 会随之变化（因为它由 hash 派生），但默认身份引用会跟着更新。

### 记住我

登录时可选，把会话从 12 小时延长到 **7 天**：

- **默认关闭**，让短会话成为一次决定而不是兜底；
- 服务端按会话记录自己的 TTL，cookie 的 `Max-Age` 与之对齐 —— 两者不一致会让浏览器继续出示服务端已丢弃的 cookie，表现为「莫名其妙被登出」；
- `/admin/api/session` 会**重新签发 cookie**（新的 `Max-Age`）。会话过期在服务端是滑动的，但浏览器的 cookie 不会自己续期；不重发的话，一个开了一天的控制台会被浏览器在中途丢掉 cookie。

### Cookie 属性

| 属性 | 值 | 理由 |
|---|---|---|
| `HttpOnly` | 是 | 管理台的 XSS 不该能偷走会话 |
| `SameSite` | `Strict` | 跨站请求根本不带它，CSRF 在源头消失 |
| `Path` | `/admin` | 不发给 `/v1/*` 与 `/api/*` |
| `Secure` | 反代是 HTTPS 时 | 由 `X-Forwarded-Proto` 判定；纯回环调试时不设，否则浏览器拒收 |

> **`SameSite=Strict` 让 CSRF 令牌变成多余的。** 不加它就需要一套 double-submit
> 或同步令牌；加了之后，任何来自其他站点的表单提交/POST 都不携带这个 Cookie，
> 攻击者无法借用受害者的会话。代价是用户从外部链接点进管理台时首次请求不带
> Cookie，会落到登录页——可接受。

---

## 路由总览

所有路由挂在 `/admin/api` 下，除 `login` 外全部要求会话 Cookie。
未登录返回 `401 {"error":{"type":"unauthorized"}}`。

### 会话

| 方法 | 路径 | 说明 |
|---|---|---|
| `POST` | `/admin/api/login` | `{token}` → 设 Cookie |
| `POST` | `/admin/api/logout` | 清会话 |
| `GET` | `/admin/api/session` | 当前会话信息 + 是否需要登录 |

### 状态与观测

| 方法 | 路径 | 说明 |
|---|---|---|
| `GET` | `/admin/api/status` | 运行状态、版本、端口、上游地址、uptime |
| `GET` | `/admin/api/stats` | 今日统计（同 `/api/stats/today`） |
| `GET` | `/admin/api/request-logs` | 请求日志分页（查询参数同 `/api/request-logs`） |
| `GET` | `/admin/api/logs` | 内存日志缓冲 |
| `POST` | `/admin/api/logs/clear` | 清空日志缓冲 |
| `GET` | `/admin/api/session-metrics` | 会话速度对比 |

### 配置

| 方法 | 路径 | 说明 |
|---|---|---|
| `GET` | `/admin/api/settings` | 当前设置（`api_key` 已脱敏） |
| `POST` | `/admin/api/settings` | 保存设置 |
| `GET` | `/admin/api/providers` | 内置厂商预设 |
| `GET` | `/admin/api/models` | 拉取上游模型列表 |

### 身份池（上游凭据）

| 方法 | 路径 | 说明 |
|---|---|---|
| `GET` | `/admin/api/identities` | 账号 + 密钥的合并列表（含交错顺序） |
| `POST` | `/admin/api/credentials` | 添加登录态 |
| `POST` | `/admin/api/credentials/:id/relogin` | 重新绑定 |
| `DELETE` | `/admin/api/credentials/:id` | 删除 |
| `PATCH` | `/admin/api/credentials/:id` | `{enabled}` 启停 |
| `POST` | `/admin/api/keys` | 添加上游 API key |
| `DELETE` | `/admin/api/keys/:id` | 删除 |
| `PATCH` | `/admin/api/keys/:id` | `{enabled}` 启停 |
| `POST` | `/admin/api/identities/default` | `{id}` 设默认 |
| `POST` | `/admin/api/identities/order` | `{order:[id...]}` 重排 |
| `POST` | `/admin/api/identities/sticky-reset` | 清会话粘滞 |
| `GET` | `/admin/api/preferences` | 打卡/成长偏好 |
| `POST` | `/admin/api/preferences/checkin` | `{enabled,time}` |
| `POST` | `/admin/api/credentials/:id/checkin` | 单账号打卡 |
| `POST` | `/admin/api/checkin` | 全量打卡 |
| `POST` | `/admin/api/growth` | 全量成长任务 |
| `POST` | `/admin/api/credentials/:id/growth` | 单账号成长 |
| `GET` | `/admin/api/growth/preview` | 成长任务预览 |

### 入站访问账户

| 方法 | 路径 | 说明 |
|---|---|---|
| `GET` | `/admin/api/client-accounts` | 列表 + 开关状态 |
| `POST` | `/admin/api/client-accounts` | 新建，**响应是明文 key 唯一一次出现** |
| `PATCH` | `/admin/api/client-accounts/:id` | `{enabled}` |
| `DELETE` | `/admin/api/client-accounts/:id` | 删除 |

### 账号池导入导出

| 方法 | 路径 | 说明 |
|---|---|---|
| `GET` | `/admin/api/pool/export` | 下载导出文件（查询参数：`encrypt`、`password`）|
| `POST` | `/admin/api/pool/import/preview` | 预览导入包 |
| `POST` | `/admin/api/pool/import` | 执行导入（`mode=merge\|replace`）|

> 桌面版走原生「另存为」面板，服务器版改为 **HTTP 下载**：浏览器自己决定存到
> 哪，不需要服务端知道路径，也就不需要给服务端写任意路径的能力。

### 与桌面端相关的配置写入

| 方法 | 路径 | 说明 |
|---|---|---|
| `GET` | `/admin/api/config/claude` | 读 `~/.claude/settings.json` |
| `POST` | `/admin/api/config/claude` | 写入 |
| `GET` | `/admin/api/config/codex` | 读 `~/.codex/config.toml` |
| `POST` | `/admin/api/config/codex` | 写入 |
| `GET` | `/admin/api/config/dsh` | 读活动 profile |
| `POST` | `/admin/api/config/dsh` | 写入 |

> 这几条在服务器上通常是**写进别人机器的配置**，用处有限。保留是因为
> 「网关和客户端跑在同一台 Linux 上」是真实场景（比如现成的开发机），
> 此时一键写入 `~/.claude/settings.json` 仍然是最省事的路径。

---

## 桌面端专有、服务器版**不做**的能力

| 桌面能力 | 服务器版 | 原因 |
|---|---|---|
| 启动/停止服务 | **无** | 进程生命周期归 systemd；「停止服务」= `systemctl stop` |
| 开机自启开关 | **无** | 归 `systemctl enable` |
| 打开日志目录 | **无** | 服务器没有文件管理器 |
| 「另存为」导出 | 改为 HTTP 下载 | 见上 |
| 扫码登录（OAuth） | **不做** | 需要浏览器回跳到本机 `localhost` 回调端口，服务器上没有浏览器；改用「粘贴登录态」 |
| 托盘、窗口、快捷键 | **无** | 不存在 |

这些不是遗漏。把它们做成「按钮点了没反应」比没有按钮更糟。

---

## 错误形状

与 `/api/*` 一致，便于前端统一处理：

```json
{ "error": { "type": "unauthorized", "message": "…", "code": "…" } }
```

| HTTP | `type` | 含义 |
|---|---|---|
| 401 | `unauthorized` | 无会话或会话过期 |
| 403 | `forbidden` | 有会话但操作不被允许 |
| 404 | `not_found` | 账号/凭据不存在 |
| 409 | `conflict` | 例如删除正在使用的默认身份 |
| 422 | `invalid_request` | 参数不合法 |
| 503 | `admin_disabled` | 未配置 `PROXY_API_TOKEN` |
