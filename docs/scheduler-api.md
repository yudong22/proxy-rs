# 定时任务观测接口（`/api/scheduler`）

网关内部有三个后台循环（每日打卡 / 登录态巡检 / 成长任务）。它们**跑在桌面进程里**，
不是 DSH Session，因此不该塞进 DSH 的 `ctx.schedule`；但它们此前只有一个 1000 行的内存
日志环 —— 进程一重启，谁都说不清「上次到底跑了没、什么时候跑的、结果如何」。

这两条路由把那个状态变成可查询的事实。外部 agent 通过一个薄工具读它（见
`dsh-plugin-proxy-gateway`），网关自己**不承担任何调度职责**：它只报告自己的循环。

## 命名空间与鉴权

- 挂在 `/api/*`，不是 `/v1/*`：`/v1/*` 是上游客户端寻址的协议命名空间，一条
  `/v1/scheduler` 会被误读成某个兼容协议的一部分（见 `src/api.rs` 模块文档）。
- 读写分离。`GET` 与既有的两个观测端点同权限（无鉴权，只绑 `127.0.0.1`）；
  `POST` **必须**带令牌，否则拒绝。

### `POST` 的令牌

`PROXY_API_TOKEN`（进程环境变量或 `~/.proxy-rs/.env`）配置后才启用写路由：

| 情况 | 响应 |
|---|---|
| 未配置 `PROXY_API_TOKEN` | `403` `{"error":{"type":"mutation_disabled"}}` |
| 令牌缺失 / 不匹配 | `401` `{"error":{"type":"unauthorized"}}` |
| 请求带 `Origin` 头 | `403` `{"error":{"type":"browser_origin"}}` |

「带 `Origin` 就拒」不是洁癖：浏览器对跨域 `POST` 一定会带 `Origin`，而合法的本机
调用方（Node 进程、curl）从不带。既然 `/api/*` 的 CORS 是 `allow_origin(Any)`，
这条规则把「你随便打开的一个网页能触发网关的账号操作」变成「不能」，成本只有一行。
令牌改动需要重启监听（`build_app_router` 在启动时读取一次）。

## `GET /api/scheduler`

只读快照。**不受** `ServiceController::is_running` 影响（与 `/api/*` 其余端点一致），
但监听停了就没人应答 —— 这是可达性，不是门禁。

```json
{
  "now": "2026-10-26 22:30:00",
  "now_ms": 1793034600000,
  "utc_offset_secs": 28800,
  "process": "proxy-rs-gui",
  "note": "这三个循环跑在 Proxy RS 桌面进程里：退出应用即停止，下次启动会补跑当天未完成的批次。",
  "loops": [
    {
      "id": "checkin",
      "title": "每日打卡",
      "enabled": true,
      "cadence": { "kind": "daily", "at": "09:00", "tick_secs": 60 },
      "last_run": {
        "at": "2026-10-26 09:00:12",
        "at_ms": 1792986012000,
        "outcome": "ok",
        "summary": "定时打卡完成: 共 3 个账号，成功 2 个，已打卡 1 个，失败 0 个",
        "manual": false
      },
      "last_skip": {
        "at": "2026-10-26 22:29:00",
        "at_ms": 1793034540000,
        "outcome": "skipped",
        "summary": "今日已打卡，等待明天",
        "manual": false
      },
      "next_due": { "kind": "at", "at": "2026-10-27 09:00", "in_secs": 37800 },
      "detail": { "accounts": 3, "checked_in_today": 3 }
    },
    {
      "id": "login_scan",
      "title": "登录态巡检",
      "enabled": true,
      "cadence": { "kind": "every", "every_secs": 300, "tick_secs": 300 },
      "last_run": {
        "at": "2026-10-26 22:25:03", "at_ms": 1793034303000,
        "outcome": "idle", "summary": "无到期账号", "manual": false
      },
      "last_skip": null,
      "next_due": { "kind": "within", "at": null, "in_secs": 300 },
      "detail": { "accounts": 3, "due_now": 0 }
    },
    {
      "id": "growth",
      "title": "成长任务",
      "enabled": false,
      "cadence": { "kind": "every", "every_secs": 3600, "tick_secs": 3600 },
      "last_run": null,
      "last_skip": {
        "at": "2026-10-26 22:14:00", "at_ms": 1793033640000,
        "outcome": "skipped",
        "summary": "成长任务自动化未开启（默认关闭，需手动开启）",
        "manual": false
      },
      "next_due": { "kind": "disabled", "at": null, "in_secs": null },
      "detail": {
        "day_pass_done_today": false,
        "night_pass_done_today": false,
        "night_window_open": false
      }
    }
  ]
}
```

### 字段口径（这几条是刻意的）

- **`last_run.at` 是真实发生的时刻**，来自 `scheduler-state.json`，不是从偏好里的
  「今天跑过了」日期倒推出来的。没有记录就是 `null`，绝不编造。
- **`last_run` 与 `last_skip` 是两个槽**，不是一个。这三个循环 tick 得很密但绝大多数
  tick 都在**拒绝干活**（打卡循环每 60 秒醒一次，几乎每次都答「还没到点」）。如果
  跳过会覆盖运行记录，操作员最想知道的「上次真的干活是什么时候、结果如何」就会被
  一个只能证明定时器活着的跳过时间戳顶掉。所以：
  - `last_run` —— 上次真正尝试干活（`ok` / `idle` / `failed`），即「上次运行」；
  - `last_skip` —— 上次被闸门挡下的时间与原因（`skipped`），即「为什么现在没在跑」。
  相同的跳过原因只记一次（否则关掉的循环会每分钟重写一次状态文件）。
- **`next_due.kind`**：
  - `at` —— `daily` 类，相位是确定的（本地 `HH:MM`），可给准确时刻；
  - `within` —— `every` 类，相位锚在进程启动时刻，HTTP 侧无法得知下一次 tick 的
    确切时刻，因此只给上界 `in_secs = every_secs`，`at` 恒为 `null`。
    **不要**把 `within` 读成「还有 N 秒」的精确倒计时；
  - `disabled` —— 该循环被关掉，没有下一次；`at` 与 `in_secs` 都是 `null`。
- **`enabled: false` 的循环仍然在 tick**，只是每次 tick 读一眼开关就返回（无 I/O）。
  因此它会持续更新 `last_skip` —— 这正好证明循环活着，而不是启动时崩了。
- **`outcome`**：`ok` / `idle`（跑了但没有需要做的事）/ `skipped`（开关关闭，
  或被当天已跑标记挡住）/ `failed`（循环自己捕获到错误）。
- **`manual: true`** 表示这条记录来自手动触发。手动运行**不动**偏好里的
  `last_checkin_run_date` / `last_growth_run_date`，所以手动跑一次不会吞掉当天的
  自动批次 —— 但也因此，`last_run.manual = true` 不能当作「定时器按时跑过」的证据。

## `POST /api/scheduler/run/{loop}`

`loop` ∈ `checkin` | `login_scan` | `growth`。

立即返回 `202`，任务在后台跑完并写进 `scheduler-state.json`：

```json
{ "accepted": true, "loop": "checkin", "run_id": "1790000000123-1",
  "note": "已在后台开始；结果会出现在 GET /api/scheduler 的 last_run。" }
```

| 情况 | 响应 |
|---|---|
| 该 loop 正在跑 | `409` `{"error":{"type":"already_running"}}` |
| 未知 loop | `404` `{"error":{"type":"unknown_loop"}}` |
| 已受理 | `202` |

**手动触发是 `forced` 语义**：跳过「今天已经跑过」的本地标记，与 GUI 上的手动按钮
一致（用户显式要求，就不该被本机记录挡住）。它**不会**改动
`last_checkin_run_date` / `last_growth_run_date`，所以不会吞掉定时批次当天的槽位 ——
这两件事必须分开，否则手动点一次就等于当天不再自动跑。

`login_scan` 手动触发只续期**当前到期**的账号（`needs_refresh`），与后台巡检同语义。
`growth` 手动触发跑**日间批次**；夜间批次（`black_cat`）只由 23:00–08:00 的窗口决定，
不提供手动入口。

## 为什么不做 `enabled` 的写接口

开关归 GUI 的偏好文件（`workbuddy-pool.json`），它有两个写者（GUI + 后台循环）就已经
需要小心；再开一条 HTTP 写路径，就得为那个文件引入事务。观测 + 触发已经回答了真正
的问题（「在跑吗 / 跑了没 / 结果如何 / 现在能不能手动补一次」），改开关仍然回 GUI。
