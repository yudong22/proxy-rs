# proxy-rs v2.3.0 发布说明：后台定时循环可观测，并可被 agent 读取与手动触发

> 版本：**2.3.0**（上一版 2.2.2）
> 新增 `GET /api/scheduler` 与 `POST /api/scheduler/run/{loop}`；给三个后台循环补上了
> 持久化的「上次运行 / 上次跳过」记录。**没有改动任何 `/v1/*` 协议路由**，对现有客户端零影响。

---

## 背景

网关里有三个 tokio 后台循环（每日打卡 60s、登录态巡检 300s、成长任务 3600s），
从 v1.8 起就在跑。但它们此前**只有一个 1000 行的内存日志环**作为痕迹：环由
`logs/proxy.log` 播种、按容量滚动，所以「今天 09:00 的打卡到底跑了没、结果如何」
几个小时后就查不到了。更麻烦的是干净机器上「手动补跑过」与「定时跑过」在日志里长得一样。

用户的实际诉求是「让各个 agent 的定时能被统一查看」。调研后结论是：**agent 侧的定时
该归 agent 自己**（DSH 有 host 级的 `ctx.schedule` + 跨 Session 的 Automation tasks 页，
每个任务绑定 `sessionId`、能恢复冷 Session、有投递回执），网关不该也不能当那个真源——
它随用户退出而停，MCP 协议本身也无法主动唤醒 agent。网关唯一该负责的「统一」是：
**把自己这三个循环的状态说清楚，并允许显式补跑一次**。

## 改动

### 1. `scheduler-state.json`：每个循环两个记录槽

新增 `~/.proxy-rs/scheduler-state.json`。**为什么是两个槽而不是一个**：这些循环 tick
得很密但绝大多数 tick 都在拒绝干活（打卡循环每 60 秒醒一次，几乎每次都答「还没到点」）。
若跳过覆盖运行记录，操作员最想知道的「上次真的干活是什么时候」就会被一个只能证明
定时器活着的时间戳顶掉。所以：

| 槽 | 装什么 | 回答问题 |
|---|---|---|
| `last_run` | `ok` / `idle` / `failed` | 上次运行 |
| `last_skip` | `skipped` + 原因 | 为什么现在没在跑 |

相同跳过原因只落盘一次（否则关掉的循环每分钟重写一次文件）。写入走 temp + rename
原子替换，并加进程锁——三个循环在不同 task 上、完成时刻互不相关，无锁的
read-modify-write 会让后写者静默吞掉前者的记录。

**这两个槽不参与任何调度决策**。「今天已跑过」仍由
`workbuddy-pool.json` 的 `last_checkin_run_date` / `last_growth_run_date` 判定：
把观测记录混进行为闸门，是「遥测写坏了开始跳过真实工作」的标准开局。

### 2. `GET /api/scheduler`

三个循环的开关、节奏、上次运行、上次跳过、下次到期、detail。只读、无鉴权（与
`/api/*` 其余端点同权限），**不受** `is_running` 影响。

字段口径里三条是刻意的不撒谎：

- `last_run` 没记录就是 `null`，绝不从「今天跑过了」倒推一个时刻；
- `every` 类循环的 `next_due` 是 `within`（只给上界），因为相位锚在**进程启动时刻**，
  HTTP 侧拿不到确切 tick 时间。写成倒计时就是把抖动变成「漏跑」；
- `enabled: false` 的循环 `next_due.kind = "disabled"`，不给假倒计时。

### 3. `POST /api/scheduler/run/{loop}`：手动补跑

`loop` ∈ `checkin` / `login_scan` / `growth`，`202` 立即返回、任务后台跑。**这是
`/api/*` 下唯一的写路由，因此单独加了闸**（见下）。语义：

- **forced**：跳过「今天已经跑过」的本地标记，与 GUI 的手动按钮一致；
- **绝不写** `last_checkin_run_date` / `last_growth_run_date`——手动跑一次不能吞掉
  当天的自动批次；
- 每循环同时只允许一个手动运行（`409 already_running`），`Drop` guard 释放，
  依赖 panic 也不会永久锁死；
- `growth` 只跑日间批次；夜间 `black_cat` 批次没有手动入口（它由 23:00–08:00 窗口决定）。

### 4. 写路由的鉴权

`/api/*` 原本无鉴权且 CORS `allow_origin(Any)`，这在只读时是可接受的取舍。一旦有
一条路由能让网关对上游账号动手，同一边界就等于「你随便打开的网页可以触发账号操作」。
所以：

| 情况 | 响应 |
|---|---|
| 未配置 `PROXY_API_TOKEN` | `403 mutation_disabled`（默认姿态不提供写能力） |
| 令牌缺失/不匹配 | `401 unauthorized` |
| 请求带 `Origin` | `403 browser_origin` |

「带 Origin 就拒」是这里真正起作用的一条：CORS 只限制**读回包**，不限制请求发出去，
所以浏览器照样能 POST；而合法的本机进程（Node 插件、curl、launchd）从不带 Origin。
令牌从 `~/.proxy-rs/.env` 或进程环境读取（Finder 启动的 `.app` 只能靠前者），
只在监听启动时读一次，改完需重启。

## 配套：`dsh-plugin-proxy-gateway`

另起一个仓库 `/Users/denis/Sites/dsh-plugin-proxy-gateway`（不随本次 tag 发布），
把上述两条路由包装成两个 DSH 工具：

- `proxy_gateway_scheduler` —— 读快照并渲染成中文；
- `proxy_gateway_scheduler_run` —— 请求补跑一次。

渲染的重点是把**四种「没事」和一种「有事」分开**：开关=关 / 空闲 / 已跳过 / 失败 /
从未运行。把 `enabled:false` 读成「坏了」、把 `idle` 读成「做成了」，是这个界面
最容易犯的两个反向错误。

## 验证

- `cargo test`：**552 单测 + 33 集成全绿**（新增 20 个单测 + 8 个 `tests/api_scheduler.rs` 集成测试）；
- `cargo clippy --all-targets -- -D warnings` 与 `cargo fmt -- --check` 干净；
- `ui/tools/check-ui.mjs` / `ui/tools/smoke.mjs` 全过；
- 真机端到端：起真 router → 真 HTTP → 真 agent 会话调用 `proxy_gateway_scheduler`，
  模型正确区分了「手动触发 / 空闲 / 从未运行」；鉴权四种状态逐一验证；
  `/v1/scheduler` 为 404、`GET /v1/messages` 仍为 405，确认协议命名空间未受影响。

## 过程中修掉的两个 bug

1. **路由参数语法**：axum 0.7 是 `:loop_id` 而非 `{loop_id}`。写错时路由注册不报错，
   但永不匹配——快照能通、触发必 404 的半坏状态，靠集成测试当场抓到。
2. **`PoolPreferences` 派生的 `Default` 不带 `09:00`**：`#[serde(default = ...)]` 只
   管反序列化，而 `load_preferences()` 在文件不存在时返回 `Default::default()`
   ——即每次全新安装。结果 API 曾发布 `"cadence": {"at": ""}`。已改为手写 `Default`
   并补两个回归测试。

## 升级说明

- 无破坏性改动，`/v1/*` 一字未动；不配置 `PROXY_API_TOKEN` 时行为与 2.2.2 完全一致。
- 新增文件 `~/.proxy-rs/scheduler-state.json`，损坏或缺失会退化为「没有记录」而不是报错。
- 手动补跑需在 `~/.proxy-rs/.env` 写 `PROXY_API_TOKEN=<随机串>` 并重启网关。
