# proxy-rs v2.4.0 发布说明：会话血缘（子智能体成本归属父会话）

> 版本：**2.4.0**（上一版 2.3.0）
> 概况页的会话指标现在会把子智能体（subagent）的开销折进派出它的会话，并把两个自动化
> 开关合并进一个可关闭的弹窗。**`/v1/*` 协议路由一字未动。**

---

## 背景

概况页「输出速度」卡片列近 3 个会话，但一套 DSH 配置跑下来会看到这样的列：

```
dsh:10b430c6…  296 次   ← 主会话
dsh:12d59abc…  119 次   ← subagent（plugin-builder）
dsh:84d1b989…   53 次   ← subagent（dsh-schedule-auditor）
dsh:6650bae7…   21 次   ← subagent（gateway-capability-auditor）
```

这不是统计 bug：`GROUP BY session_id` 忠实履行了它被告知的口径。真正缺的是**网关拿不到
父子关系**——DSH 内部有 `session.header.parentSession`，但适配器只发会话 id，不发它。

靠时间重叠去猜是错的：子会话可能先于父会话结束，父会话也可能同时跑多个子代理。
必须由客户端显式告知。

## 改动

### 1. `parent_session_id`：一个可选的请求头

新增对 `x-deepseek-harness-parent-session-id` 的解析（`src/session.rs`），值按与
`session_id` **同一把尺子**校验后加命名空间前缀存入 `dsh:<value>`。头缺失 ⇒ 空 ⇒
该会话按顶层计，**老客户端完全不受影响**。

对应的 DSH 侧改动（4 文件 25 行，另仓维护）：`GenerateOptions` 加 `parentSessionId?`，
agent loop 在唯一一处打标，`llm-pi-ai` 适配器发头。打标条件是
`origin === 'subagent'`，**不是**「有 `parentSession` 就发」——普通 fork 也可能带着它，
而 fork 是另一段对话，把它的开销折回来源会话回答的就不是「这次对话花了多少」了。

### 2. 汇总口径：一个顶层会话 + 它的所有子会话

`request_logs` 新增 `parent_session_id` 列（走既有 `migrate()` 的 `PRAGMA table_info`
探测 + `ALTER`，与其余列同一路径）。三个查询改了口径：

- `query_session_metrics_on` / `query_latest_session_metrics_on` —— 走新的
  `fold_session_tree_on`；
- `query_recent_session_metrics_on` —— 只列顶层会话（`parent_session_id = ''`），
  子会话不再单独占一列。

三条刻意的约束：

1. **只合并一层。** 孙会话的 `parent_session_id` 指向它自己的父级，逐层汇总天然正确；
   递归会在数据有环时挂死。
2. **工具等待时间按会话分别算再相加。** `fold_session_turns` 把「上一轮以工具调用收尾」
   到「本轮开始」的间隔算作工具执行时间，父子的轮次是交错的两条时间线，拼在一起排序会
   凭空造出不存在的工具等待。
3. **同屏两个数字必须用同一口径。** 当前会话列与对比列都走树干聚合。

### 3. 修掉一个自己造的 bug

`latest_session_id_on` 取「最新一条记录」却**不排除子会话**——而子智能体的请求几乎
总是最新的。结果顶部「当前会话」卡片会套用父会话的标签、显示某个子智能体的数字。
已加 `parent_session_id = ''` 过滤 + 回归测试
`the_latest_session_is_never_a_subagent_child`。

### 4. 「上次运行」行（速度卡下钻表）

`last_at` 一直在 payload 里却从没渲染过，导致一列「今天」的聚合数字读起来像是实时的：
两个会话 TPS 相同也看不出其中一个已经闲置几小时。现在按行显示：

- 今天的轮次显示为裸时钟（`14:32`）；
- 更早的保留日期（`10-26 14:32`）；
- 未测量显示 `—`，绝不编造。

拆字符串而不是用 `new Date()`：值是**无时区的本地墙钟字符串**，`new Date` 会当成 UTC
再按本机偏移平移。

### 5. 两个自动化开关合并进一个弹窗

「每日定时打卡」和「自动跑成长任务」是同一个决定（这台机器无人值守时做什么），
之前却分散在账号池区块的两处、各有各的保存。现在合并为「⏱ 自动化设置」按钮 + 弹窗，
一次保存。

**页面上始终保留一行摘要**（如「已开启：每日打卡 08:30 · 成长任务」）——关掉弹窗后
开关就不可见了，而一个看不见的无人值守任务正是用户会忘记的那个。

## 待办（已知）

回撤 DSH 侧改动时，子智能体的 **compaction 摘要**与**会话标题**调用没有带上父 id，
因此这两类低频调用仍会被记成独立的顶层会话。网关侧已正确处理（有父 id 才折、没有就当
顶层），所以不会算错，只是这部分不折进父会话。

## 验证

- `cargo test`：**592 通过**（新增会话血缘、聚合、迁移与「当前会话」回归共 10 个测试）；
- `cargo clippy --all-targets -- -D warnings`、`cargo fmt -- --check` 干净；
- `ui/tools/check-ui.mjs` / `ui/tools/smoke.mjs` 全过（smoke 新增弹窗开合与开关状态断言）；
- DSH 侧：1936 测试通过、`tsc -b` 干净、oxlint 0 警告、生成物门禁绿。

## 升级说明

- 无破坏性改动，`/v1/*` 未动。
- `stats.db` 自动迁移新增一列，历史行为空字符串——它们**不可回填**（旧请求从未携带过
  这个信息），因此历史会话的汇总只包含「自本版本起」的子智能体。
- 未升级 DSH 时一切照旧，只是不会折叠。
