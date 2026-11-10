# proxy-rs v3.5.5 发布说明：修好前端开发循环

> 版本：**3.5.5**（上一版 3.5.4）
> **产品行为零变更。** 这一版只动 `vite.config.js`、新增 `ui/tools/dev-server.mjs`，
> 都是 `ui-dev` 本地开发用的，不进任何发布产物。

一句话：`task ui-dev` 这条开发链路此前**基本不可用**——它要么连不上后端，要么
连上了但整页都是 mock，要么进了控制台却找不到切回测试/生产的入口。这些问题在
同一轮里被发现并修掉，因为它们互相掩盖。

---

## 1. 「4300 全是 mock」

`ui-dev` 打印的地址是 `http://127.0.0.1:4300/`，而这个路径由 Vite 直接提供
`ui/index.html` 源文件——**没有网关注入的 bridge，也不在 `/admin` 下**。

控制台按 `location.pathname.startsWith('/admin')` 决定传输层
（`ui/src/core/ipc.js`），所以从 `/` 进入时每一次 `invoke` 都落到最后一层 no-op 兜底：

```
[Tauri Mock] Invoke 'get_settings':
```

页面能渲染，每个面板都是空的。`core/api.js` 的 `apiBase()` 同样按 `/admin` 判断，
所以 `/api/*` 的观测读也一起退化了。**实际上并没有用到 mock 数据，只是被错误的入口
URL 带进了 mock 分支。**

修法：`/` 重定向到 `/admin/`。这不是让 `/` 勉强能用，而是因为控制台只有那一个
真实入口——网关只在 `/admin/` 提供页面、cookie 的 `Path` 是 `/admin`、自动登录
也挂在 `/admin/` 上。两个行为不同的入口比只有一个更糟。

---

## 2. 连不上后端：`task` 没装

`Taskfile.yaml` 写着 `task dev-server`，但 go-task 不是默认安装的。缺了它的表现
和「后端挂了」一模一样：

```
[vite] http proxy error: /api/stats/today
Error: connect ECONNREFUSED 127.0.0.1:4457
```

因为 4457 上根本没有东西在听。

修法：新增 `npm run dev-server`（`ui/tools/dev-server.mjs`），与 task 版等价——
构建一次、把 `$HOME/.proxy-rs-dev/gui-settings.json` 的端口写成
`PROD_PORT + DEV_PORT_OFFSET`、带 `PROXY_API_TOKEN=dev` 前台运行。另加
`npm run dev-hmr` 作为 `ui-dev` 的等价入口。

> 那个 `gui-settings.json` 不是顺手写的：网关从它读端口，残留旧值正是两边端口
> 漂移的来源（这个仓库真出过）。

---

## 3. 每次都要手输 token

测试后端是 headless 网关，`require_session` 对所有 RPC 要求会话；而 token 只有
`Taskfile.yaml` 知道：`dev`。于是每换一个 Vite origin、每次清 cookie，都要在
登录页手输一次——热更新循环里反复付这笔账。

修法：在 Vite 代理**之前**拦截控制台页的 GET，用一个带 dev token 的
`POST /admin/login` 把网关自己下发的会话 cookie 原样转给浏览器。**没有绕开任何
鉴权**：会话仍由网关铸造，dev 循环只是把它已知的 token 填上。

安全边界：

- 只对测试后端（`wantsProd` 先短路；生产 App 没有 token 可提交）
- 只对 loopback（再查一次 `remoteAddress`，不只依赖 `server.host` 配置）
- 只对 `/admin/`、`/admin` 的 GET（精确排除 `/admin/login`，否则没法看登录页本身）
- `DEV_AUTO_LOGIN=0` 可关掉
- 构建产物中零命中（已实测 `ui-dist` 内无相关字符串）

### 其中两个坑值得记下来

**坑一：判据必须是「验证」而不是「检测」。** 第一版写的是「有没有
`proxy_rs_admin` cookie」，在最需要它的场景恰好失效：dev 网关的会话表在内存里，
**重启网关就作废它发过的所有 cookie**，而浏览器仍把旧 cookie 送上来。判据认为
「已登录」→ 自动登录不执行 → 网关返回登录页 → 又要手输。

改为拿 cookie 去打 `GET /admin/api/session`（有效 200 / 失效 401），只有确认失效
才重登。代价是每次页面加载多一次 loopback 请求，换来「重启网关 → 刷新页面 → 直接进」。

**坑二：登录成功返回的是 303，`upstream.ok` 是 false。** 第一版按 `ok` 判断，
于是每次成功的会话 cookie 都被静默丢掉——表现成「功能没写」，而不是「状态码判错」。
改为认 302/303（或有 `Set-Cookie` 的 2xx）。

---

## 4. 切到生产后回不来

DEV 徽标（右下角，测试/生产切换）原来靠 `transformIndexHtml` 注入。这个钩子**只对
Vite 自己处理的页面生效**，也就是 `/` 下的 `ui/index.html`。而 `/admin/` 的页面是
**网关**返回的，Vite 只做转发，HTML 管线从头到尾没见过它。

第 1 节把 `/` 重定向掉之后，徽标就被注入进了一个再没人加载的文档里——切换入口消失。

修法：改在**代理响应**这一层注入（`proxyWithBadge`）。这是唯一同时看得见「这是
一个控制台页 GET」和「网关返回的 HTML」的位置。只改写 2xx 的 HTML：302 到登录页、
错误、资源流一律原样透传（在登录页上挂一个按了也不生效的开关，比没有更糟）。

两个细节：

- 改写后必须**重算 `Content-Length`**，否则浏览器按原长度截断，徽标被切掉。
- **两个后端都要注入，不能只在 dev 注入。** 徽标承载的是唯一能切回测试后端的入口，
  只注入 dev 页等于把开关做成单向的：切到生产后下一次加载没有按钮，测试后端从 UI 里
  彻底够不着——一次点击就困住了。

---

## 验证

全部实测，不是推断：

| 检查 | 结果 |
|---|---|
| `GET /` | 302 → `/admin/` |
| `GET /admin/` | 302 + `Set-Cookie`（自动登录） |
| 无 cookie / 失效 cookie / 有效 cookie | 依次：自动登录 / 重新登录 / 直接进控制台 |
| 重启 dev 网关后用旧 cookie | 302 自动恢复会话（坑一） |
| RPC `get_settings` | 真实数据（`actual_port=4457`、真实 `chat_url`），非 mock |
| 控制台页完整性 | `</html>` 正确闭合，`Content-Length` 与改写后字节数一致 |
| dev 后端 | 徽标 2 处，`data_dir=~/.proxy-rs-dev` |
| **prod 后端** | **徽标 2 处**，`data_dir=~/.proxy-rs`（双向可切，数据确实来自不同后端） |
| `DEV_AUTO_LOGIN=0` | 回到真实登录页 |
| 构建产物 | 徽标 / dev token 相关字符串 **0 命中** |

- `cargo test`：**802 passed / 0 failed**
- `cargo fmt --check` / `cargo clippy --all-targets -- -D warnings` 干净
- `check:ui` / `check:dist` / `build:ui` / `smoke` 全过

---

## 怎么用

```bash
npm run dev-server   # 终端一：headless 网关（4457，token=dev）
npm run dev-hmr      # 终端二：Vite 热更新（4300）
```

打开 **`http://127.0.0.1:4300/`**（会 302 到 `/admin/` 并自动登录），右下角 DEV
徽标切测试/生产。`task dev-server` / `task ui-dev` 仍然可用。

无配置格式变更，无行为变更。
