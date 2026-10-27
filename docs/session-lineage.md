# 会话血缘：让网关知道「这是谁的子智能体」

## 问题

DSH 会把子智能体（subagent）的请求打在**自己的** `x-deepseek-harness-session-id` 上，
而网关只认这一个头。于是概况页的「最近 3 个会话」把父会话和每个子智能体各列一列：

```
dsh:10b430c6…  296 次   ← 主会话
dsh:12d59abc…  119 次   ← subagent（plugin-builder）
dsh:84d1b989…   53 次   ← subagent（dsh-schedule-auditor）
dsh:6650bae7…   21 次   ← subagent（gateway-capability-auditor）
```

这不是统计 bug：`stats.rs` 的 `GROUP BY session_id` 忠实履行了它被告知的口径。
真正缺的是**网关拿不到父子关系**——DSH 内部有 `session.header.parentSession`，
但适配器只发 session id，不发它。

靠时间重叠去猜是错的：子会话可能先于父会话结束，父会话也可能同时跑多个子代理，
接续会话更是完全重叠。必须由 DSH 显式告知。

## 契约：一个可选请求头

```
x-deepseek-harness-parent-session-id: <父会话 id>
```

### 发送规则（DSH 侧，只此一条）

**仅当** `session.header.origin === 'subagent'` **且** `session.header.parentSession`
存在时才发送。两者缺一不发。

为什么不满足于「有 parentSession 就发」：`SessionHeader.parentSession` 的语义是
「直接创建者」，**普通 fork 也可能带着它**（见 `core/session/src/types.ts` 的字段注释：
"Ordinary forks omit this field; historical ordinary forks may retain their source id
here. Presence alone does not classify a Session as a subagent; use `origin` for that
distinction"）。fork 是用户主动分叉出的**另一段对话**，把它的开销折回父会话，
回答的就不是「这次对话花了多少」了。`origin === 'subagent'` 才是那个分类器。

### 解析规则（网关侧，只此一条）

`parent_session_id` 是**原始 id**（不带命名空间前缀），网关收到后按自己的规则加前缀：
`dsh:<value>`。校验与 `session_id` 同一把尺子（`looks_like_session_id`）——
一个畸形的父 id 必须被丢弃，而不是存进库里让聚合查询静默漏行。

头缺失 ⇒ `parent_session_id` 为空 ⇒ 该会话按顶层会话计。**老客户端因此完全不受影响。**

## 网关侧的数据模型

`request_logs` 新增一列：

```sql
ALTER TABLE request_logs ADD COLUMN parent_session_id TEXT NOT NULL DEFAULT ''
```

迁移走既有的 `migrate()`（`PRAGMA table_info` 探测 + `ALTER`，与其余列同一路径）。
历史行为空字符串——它们**不可回填**，因为旧请求从未携带过这个信息。这一点必须在
UI 上说清楚：历史会话的汇总只包含「自本版本起」的子智能体。

## 汇总口径

顶层会话的 token / 请求数 / 速率 = 它自己 + **所有 `parent_session_id` 指向它**的行。

三条约束：

1. **只合并一层。** 子智能体的子智能体（`delegationDepth >= 2`）的 `parent_session_id`
   指向的是它自己的父级，因此逐层汇总天然正确；不要去递归，递归会在数据有环时挂死。
2. **未测量 ≠ 0。** 子会话里若有请求没测到 TTFT/速度，汇总列不能因此变成 0——
   与既有面板同口径（`—` 表示未测量）。
3. **同屏两个数字必须用同一口径。** 细化：当前会话列与对比列都走树干聚合
   （`fold_session_tree_on`），否则同一个会话会在同一块面板上读出两个不同的 token 数
   —— 这是最糟的一种错：两个数字、一屏之内、无法判断哪个才是「这次对话」。

   实现时踩到了这一点：`query_session_metrics_on` 最初仍调用单会话的
   `fold_session_on`，测试 `a_parent_session_absorbs_its_subagent_children` 当场
   抓出父会话只折了 1 轮而不是 3 轮。已改为走树干聚合。

   顶层「当前会话」的**语义**仍是「这次对话（含它派出的子智能体）」，不再单列 ——
   子智能体已经是这次对话的一部分，单列反而会让「这次对话花了多少」无法回答。

## 兼容性

- 头是可选的，不发就是现在的行为。
- 网关列有默认值，老 `stats.db` 迁移后照常工作。
- 两仓库可独立发布：DSH 先发不会坏（网关忽略未知头），网关先发也不会坏（只是没有数据）。

## 实际落地的范围

DSH 侧只改了 `llm-pi-ai` 适配器（走 proxy-rs 的那条链路）。`llm-deepseek` 直连
`api.deepseek.com/anthropic`，它发这个头也没有任何一方消费，因此没改。

已知缺口，不打算在本次修：子智能体的 **compaction 摘要**与**会话标题**调用走的是
不同的构造路径，没带父 id，因此这两类低频调用仍会记成独立的顶层会话。网关侧对此是
正确的（有父 id 才折、没有就当顶层），所以不会算错数，只是这部分不折进父会话。
要补的话是各加一处 `parentSessionId` 展开，代价小但会扩大 DSH 侧的改动面。
