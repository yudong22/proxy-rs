# proxy-rs v3.2.2 发布说明：登录页改为路由器形态 + 管理台 CORS 收窄

> 版本：**3.2.2**（上一版 3.2.1）
> 两处修正，都源于实测反馈。桌面端行为不变。

---

## 1. 登录页：去掉 username 字段，改回单密码框

### 现象

Chrome 对路由器这类**只有密码框**的登录页会提示保存密码，但对本管理台不提示——
同一个浏览器、不同页面。

### 根因

v3.1.0 起，我为了"让密码管理器认得出登录表单"加了一个 username 字段，并在 v3.2.1
把它从 `readonly` 改成可编辑。方向是错的：

- Chrome 对**单个密码框**本来就有识别启发式（这就是它为什么能给路由器弹提示）；
- 而配上一个**固定预填值**的 username 字段之后，反而不再被当作登录表单处理。

少一个字段才是修法。

### 改动

```html
<form method="post" action="/admin/login">
  <input name="password" type="password" autocomplete="current-password">
  <button type="submit" name="login">登录</button>
</form>
```

服务端本来就只用 token 鉴权、完全忽略 username，所以移除它不影响登录。

### 关于验证方法（重要）

之前我三次"验证通过"都是错的，原因值得记下来：

| 我用的方法 | 为什么无效 |
|---|---|
| 无头浏览器断言 | 无头模式**本身不显示**保存密码气泡 |
| 检查 `Login Data` 有 40KB | 那是初始空库；实际 `logins` 表 **0 行**，我看错了 |
| 自动化提交后查询 | 自动化临时 profile 里 `password_manager_enabled` 未设置，Chrome 默认**不启用**密码管理器 |

结论：**这类浏览器行为无法用自动化验证**，只能靠真实浏览器的人工确认。现在的
`admin-smoke` 只断言表单**标记形状**（单密码框、`autocomplete=current-password`、
无 username），并在注释里写明它证明不了保存提示本身。

---

## 2. 管理台不再继承数据面的宽松 CORS

### 现象

```
GET /admin/login
access-control-allow-origin: *
```

### 根因

`build_app_router` 的 CORS 层（`allow_origin(Any)`）包住了整个 router，管理台作为
后合并进去的路由被一并覆盖。

数据面开放 CORS 是**有意的**（桌面 GUI 从 webview 读 `/api/*`，`src/api.rs` 的模块
文档记录了这笔取舍）。但管理台是不同的信任域：

- 它由**同一个监听端口**提供页面，天生同源，不需要任何跨域许可；
- 它携带会话 Cookie，返回 `*` 意味着**用户访问的任意网页**都能读到该 Cookie 所发
  请求的响应体。

`SameSite=Strict` 挡住了 Cookie 本身被读取，所以这是**配置正确性**问题、不是敞开
的大门——但那个头本来就不该出现在这些路由上。

### 修法

`build_app_router` 不再接受 `admin` 参数；新增 `router::with_admin(base, admin)`，在
`.layer(cors)` **之后**合并管理台：

```rust
let app_router = router::build_app_router(...);   // 数据面，含 CORS
let app_router = router::with_admin(app_router, Some(admin_router));  // 管理台，无 CORS
```

两条策略从此独立：以后改数据面的 CORS 故事，不会顺带放宽管理台。

新增回归测试同时钉住两侧——管理台无 `Access-Control-Allow-Origin`，`/api/stats/today`
仍返回 `*`。

---

## 3. 验收

| 项 | 预期 | 实测 |
|---|---|---|
| `cargo test` | 全绿 | ✅ 704 passed |
| `cargo clippy -D warnings` | 无告警 | ✅ |
| `cargo fmt --check` | 无差异 | ✅ |
| `admin-smoke`（含表单形状断言） | 全通过 | ✅ |
| `check-ui` / 桌面 `smoke` | 无问题 | ✅ |
| 管理台 `Access-Control-Allow-Origin` | 不存在 | ✅ |
| 数据面 `/api/stats/today` 的 CORS | 仍为 `*` | ✅ |
| 单密码框表单登录 | 303 → `/admin/` | ✅ |

> **密码保存提示本身无法自动化验证**，见 §1。这一项以真实浏览器的人工确认为准。

---

## 4. 升级

服务器：

```bash
curl -LO https://github.com/yudong22/proxy-rs/releases/download/v3.2.2/proxy-rs-server-linux-x64.tar.gz
sha256sum -c proxy-rs-server-linux-x64.tar.gz.sha256
tar xzf proxy-rs-server-linux-x64.tar.gz

systemctl stop proxy-rs
install -m 755 proxy-rs-server-linux-x64/proxy-rs-server /opt/proxy-rs/bin/
systemctl start proxy-rs
```

无需迁移数据，反代配置也不用改。

**若之前尝试过登录并被 Chrome 记住"永不保存"**，需要到
`chrome://settings/passwords` 把 `api.wuba.site` 的拒绝记录删掉，否则不会重新提示。
