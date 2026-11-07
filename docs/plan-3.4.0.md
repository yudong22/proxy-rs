# proxy-rs 3.4.0 规划：统一管理面到一套 HTTP API（方案 A）

> 基线：`feat/session-browser` @ `e901e0b`（含 S0 hotfix）
> 一句话目标：**桌面 webview 与服务器控制台共用同一个 `/admin/` 面**，
> 删除约 50 个重复的 Tauri 命令体，业务逻辑在 `admin.rs` 只写一遍。
>
> 决策记录（2026-11-07，用户拍板）：
> - 采用**方案 A**：保留 Tauri 壳 + 最小 IPC（启停/托盘/原生能力），业务命令统一到 HTTP。
> - 桌面鉴权用**进程内自动铸会话**，不做回环豁免、不强配 token。

---

## 1. 为什么可行（证据）

- `ui/server/bridge.js` 已把 `invoke(cmd,args)` 映射为 `POST /admin/api/rpc/<cmd>`，
  `src/admin_rpc.rs` 是完整分发表——**同一份 `ui/` 不改一行就能跑在 HTTP 上**（服务器版已在跑）。
- `src/console.rs` 的 `build_console_router` 服务整套 `/admin/` 页面 + 静态资源。
- 桌面唯一的缺口：`src-tauri/src/main.rs` `with_admin(app_router, None)`（3.1.0 有意不开），
  以及 webview 加载的是本地文件（`tauri://localhost`）而非同源 HTTP。

## 2. 两个硬阻塞的解法

### 2.1 鉴权：进程内铸会话（不开回环豁免）

- 桌面构造 `AdminState` 时新增字段 `desktop: bool`。
- console 路由新增 **`GET /admin/api/desktop-boot`（不挂 require_session）**：
  仅当 `state.desktop == true` **且** 请求来自 loopback 时，
  用进程内生成的随机 token 铸一个 session 并 `Set-Cookie` 返回；
  否则 404（服务器上该端点不存在般沉默，远程访问永远无法利用）。
- 桌面进程启动时生成随机 token 喂给 `AdminSessions`（内存中，随进程生灭）；
  `PROXY_API_TOKEN` 已配置时照常使用配置值——桌面行为与服务器一致。
- webview 同源加载 `/admin/` 后，页面 JS 先打一次 `desktop-boot` 拿 cookie，之后一切 RPC 正常。
- **信任域不变**：远程访问者看不到 desktop 端点的存在（404），loopback 之外即使
  误配也不铸会话；服务器版 `desktop=false`，语义与 3.2.2 完全一致。

### 2.2 自举与 listener 自锁

- **保留 frontendDist=../ui**，新增桌面专用启动页 `ui/bootstrap.html`（极简：状态 + 启动按钮），
  `tauri.conf.json` 的窗口 `url` 指向它。
- bootstrap 页流程：小 IPC `get_bootstrap_info`（port/running）→
  轮询 `desktop-boot` → 可达则拿 cookie 并 `location.replace('http://127.0.0.1:<port>/admin/')`；
  不可达则显示「服务未运行」，点「启动」走保留的 `start_service` IPC，成功后跳转。
- listener 重启（保存设置/导入等触发的热重建）期间 `/admin/` 页面短暂失联：
  页面 fetch 失败 → 显示「重连中」并轮询 desktop-boot 恢复（端口变更时跟随新 port）。
- `start_service`/`stop_service` 走**保留的 IPC**，永远不经过它自己托管的 listener。

## 3. 保留的桌面特例（最小 IPC 面，约 8 个）

| 命令 | 原因 |
|---|---|
| `get_bootstrap_info`（新） | 自举：port / running，先于 listener 存在 |
| `start_service` / `stop_service` | 控制承载 API 的 listener 本身 |
| `open_logs_dir` / `reveal_path` | 调 OS 文件管理器 |
| `pick_file`（新，替代 pool_import 的文件对话框） | 原生打开对话框选文件、读文本，内容随后走统一的 `pool_import_preview/import` RPC |
| `pool_export` | **删除**：同源后直接 `location = /admin/api/pool/export` 浏览器下载（服务器版语义） |
| 托盘 | OS 原生；菜单事件直接调进程内 start/stop，不经过 webview |

其余统一化决策：
- **`wb_oauth_start/poll` 移入 `admin_rpc`**：轮询是远程 endpoint，二维码是 `qr_svg` 数据 URI，
  web 控制台同样能展示；服务器版「不做」的原因（无浏览器）在同源 web 页面下不再成立。
  桌面版删除对应命令。
- **`service-state` / `growth-progress` 事件**：bridge 的 `listen` 是 no-op，
  统一为轮询——`get_status` 已被 10s 轮询覆盖 service-state；growth 进度新增
  `GET /admin/api/growth/progress`（轮询 job 状态）。桌面 emit 保留给托盘提示，UI 不再依赖。

## 4. 分阶段实施（每阶段独立提交、独立验证）

| 阶段 | 内容 | 验证 |
|---|---|---|
| **A1** | library：`AdminState.desktop` 字段；console 加 `desktop-boot` 端点 + `AdminSessions::create_with_token`；`admin_rpc` 补 `open_logs_dir`/`reveal_path`/`wb_oauth_*`/`growth 进度查询` | `cargo test`（新增端点测试：desktop=true+loopback 铸会话；desktop=false 或非 loopback 404） |
| **A2** | 桌面：构造 `AdminState`（含随机 token）、`with_admin(Some)`；`tauri.conf.json` 窗口 url → `bootstrap.html`；新增 `ui/bootstrap.html` + `get_bootstrap_info`/`pick_file` IPC | 手动 `task dev`：启动 → 跳转 /admin/ → 全功能可用；stop 后 bootstrap 显示未运行 |
| **A3** | UI：`core/ipc.js` 判定「同源 /admin/」时直接用 bridge 路径（合并 `bridge.js` 逻辑）；`api.js` 的 `apiBase()` 改为同源相对路径；pool-export 改浏览器下载；growth 进度改轮询；删除桌面特有 UI 分支 | `task ui-test` + 桌面手动回归 |
| **A4** | 删 `src-tauri/main.rs` 中约 50 个重复命令体（保留 §3 特例），相关 `proxy_rs::admin::*` 调用改走共享层；`server.rs` 旧注释修正 | `cargo clippy -D warnings`；命令清单守卫测试更新 |
| **A5** | 结构守卫测试（原 3.3.0 S2）：`generate_handler!` 命令集 = UI invoke 集 ∩ 特例清单；`console.rs ASSETS` 覆盖 `ui/src` 全部模块；依赖环数=0 断言 | CI 自动 |
| **A6** | 原 3.3.0 S4–S8（env.rs/store.rs/billing.rs/删 Config 死字段/admin_state.rs）→ 环 7→0 | 守卫测试 |
| **A7** | 原 S9/S10（settings 拆 4、workbuddy_auth 拆 8 + 每子模块最小单测 + 兼容层一个版本周期） | `git diff -M --stat` 全 rename |

> S1（收编 4 组重复函数）被 A4 自然覆盖（桌面命令体删除即消除重复）；
> S3（决策 A：REST 保留）维持——REST 与 RPC 共用 handler 的现状不变。

## 5. 风险与缓解

| 风险 | 缓解 |
|---|---|
| desktop-boot 端点被滥用 | `desktop==true` 才存在 + loopback 才铸会话 + 随机 token 仅在进程内存；服务器版 404 |
| listener 重启窗口期 UI 失联 | bootstrap 重连流程（§2.2）；页面级「重连中」提示 |
| 改端口后 webview 失联 | bootstrap 每次经 `get_bootstrap_info` 取当前 port |
| A4 大 diff 掩盖逻辑变化 | 每个命令删除独立可验证；守卫测试钉住命令清单 |
| 与 3.3.0 原计划冲突 | 本文档取代其 S1/S3，其余 S 编号沿用 |

## 6. 验收标准

1. `cargo clippy --all-targets -- -D warnings` 零告警；`cargo test` 全绿且只增不减（基线 730）
2. 桌面 `task dev`：bootstrap → /admin/ → 全部管理功能可用；stop 后 UI 可再启动
3. 服务器版行为与 3.2.2 完全一致（admin-smoke 全过；desktop-boot 在服务器上 404）
4. `generate_handler!` 命令数 ≤ 10 且全部在特例清单内（守卫测试）
5. 依赖环 7→0（守卫测试自动断言）

## 7. 实施记录：踩到的坑（2026-11-07）

### 7.1 bootstrap 页不能用 `fetch` 探测控制台

bootstrap 页跑在 `tauri://localhost`，对 `http://127.0.0.1:<port>` 的任何请求都是
**跨域**；而控制台 router **故意不发 CORS 头**（v3.2.2 把"管理台继承数据面
`allow-origin: *`"当作安全问题修掉了）。两者叠加的结果：webview 读不到探测响应，
即使控制台完全健康也会报"端口无响应"——而浏览器直接打开同一地址是 200。

- **正确做法**：用 IPC 问主进程。`get_bootstrap_info` 的 `console_ready` 来自
  主进程自己绑定的 listener，是权威答案，且不需要网络往返。
- **反例**：早期用 `/health` 探测能工作，只因它走数据面 router（有 CORS）——
  换成控制台端口后立刻失效。这个差异很容易被误判成"端口写错了"。

### 7.2 控制台与数据面必须是两个 listener

用户报「web 服务和 v1 代理启动都失败了，他们应该是两个独立服务」。

- 控制台承载"启动/停止服务"按钮；与数据面共用 listener 时，停止代理会关掉
  端口，连"启动"按钮一起带走——用户被锁死。
- 拆分后：控制台 `proxy port + 1`（占用则 +2…+9），进程启动即起、永不停止；
  数据面可启可停，停止时返回 503 `service_unavailable`。
- `get_bootstrap_info` 因此同时报 `port`（控制台）与 `proxy_port`（数据面），
  两者混淆过一次（见 7.3）。

### 7.3 `bound_port` 不能以 `DEFAULT_PORT` 初始化

`AppContext.bound_port` 初值曾是 `DEFAULT_PORT = 3456`。代理未绑定时它仍报
3456，而用户配置是 3457 —— bootstrap 页去探测一个从未绑定过的端口，报
「服务未在预期端口响应（端口 3456）」。初值改为 `0`（= 未监听），"端口是多少"
与"是否在听"从此是两件事。
