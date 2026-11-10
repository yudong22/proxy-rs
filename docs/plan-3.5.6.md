# proxy-rs 3.5.6 规划：会话列表二级聚合 + 会话地址可持久 + 来源可扩展

> 基线：`main` @ `7c94040`（v3.5.5）。目标版本：**3.5.6**。
>
> 一句话：**把「会话列表」从一条平铺的文件清单，变成「项目 → 会话」的两级结构；
> 把「当前在看哪个会话」从内存闭包变成可刷新、可收藏、可分享的地址；
> 把「来源」从一个手写的文案常量变成唯一有代码位置的注册表。**
>
> 上游输入（都是本基线实测，可复现）：
> [`docs/audit-3.5.6/backend-sources.md`](audit-3.5.6/backend-sources.md)（249 行）、
> [`docs/audit-3.5.6/frontend-routing.md`](audit-3.5.6/frontend-routing.md)（261 行）。
> 本文每个数字都能追到那两份的 §7 / §6 复现命令。

---

## 0. 决策记录

### 0.1 用户拍板（2026-11-10）

| # | 问题 | 决定 |
|---|---|---|
| D1 | 一级是什么 | **一级 = 项目目录，来源作为行内标签 / 顶部筛选**。同一项目下的 Claude / DSH / Codex 会话并列，不按来源切开 |
| D2 | 刷新保留的范围 | **刷新 + 切走 tab 再回来，都要回到同一会话**（不是只保证刷新） |
| D3 | 本版范围 | **只做扩展点**：来源抽象 + 路由带来源；仍然只有 Claude Code 可读。真接 DSH/Codex 留下一版 |

### 0.2 写方案时定的技术决策（可被推翻，但每条都写了理由与代价）

| # | 决策 | 理由（反对意见写在括号里） |
|---|---|---|
| T1 | 会话地址形态 `#sessions/<source>/<id>`，**旧形态 `#sessions/<uuid>` 继续接受并恒等于 `claude`** | 来源是一个枚举 token、id 按来源各自校验，两段互不污染。不用 `sessions/claude:<uuid>` 单一 token，是因为 id 规则本来就要按来源分叉（DSH 是 `session-<uuid>`、Codex 有双 uuid 陷阱），合成一段后还得再拆一次。（代价：链接多一段；已发出的老链接必须继续可用） |
| T2 | 「上次打开的会话」放 **localStorage**，URL 只承载「地址」不承载「记忆」 | D2 要求「切走再回来」——切 tab 必然把 hash 改成 `#logs`，id 留不到下一次。内存记一份只能扛同一页面生命周期，扛不住「切走→回来→刷新」这个复合动作。（代价：本仓库前端**零 Web Storage 先例**，这是新约定，要写进 README） |
| T3 | 点击一行用 `history.replaceState` 写地址，**不产生历史条目** | 不给 10 次点击塞 10 条 history；`replaceState` 不触发 `hashchange`，天然没有重入。`main.js:230` 已有先例。（代价：浏览器后退不逐会话回退，回退仍是「上一页/上一 tab」） |
| T4 | 分组键：**来源自己的项目目录**（Claude = `encoded_project`）作为身份；组上另带**规范化绝对路径 `cwd`** 作为将来跨来源合并的键 | 目录名在单一文件系统内天然唯一，stat 阶段零额外 IO；而目录名编码有损（`/a/b/c` 与 `/a-b-c` 同名，本机已存在此模糊目录），所以它**不能**当跨来源身份。3.5.6 不做合并，但先把 `cwd` 摆在 wire 上，接第二个来源时不必改契约 |
| T5 | 每组默认返回 **10** 条（`group_limit`），组级 `count`/`last_seen_ms`/`size_bytes` **全部来自 stat**，精确 | 组头要显示「10 个会话 · 2 小时前」，这必须对全量算；但行级 `turns`/`tool_calls` 仍只对命中行算（`read.rs:15-16` 的既有原则不变）。代价是读文件数从 `O(limit)` 变成 `O(min(total, 组数×group_limit))`——**本机实测这几乎不变**：本机组大小 `[10,8,4,2,2,1,1,1,1,1]`，`group_limit=10` 时读 **31** 个文件，与今天平铺 `limit=50` 读 31 个**完全相同**；合成 300 文件/24 组时是 240 个，按实测 96 ms 外推 ≈77 ms（可接受，且是上限） |
| T6 | 组默认**全部展开**，组头可点击折叠；折叠状态只存内存（本版不持久化） | 默认折叠会让「打开会话页什么会话都看不见」，也会直接打红 `smoke` 的「列表渲染 1 行」（`scenarios-views.mjs:115`）。折叠状态属于可选优化，等有人真抱怨再加 |
| T7 | 组样式**追加进 `ui/style/view-sessions.css`**，不新建 CSS 文件 | 新建样式文件要四处同步（`ui/index.html`、`check-ui.mjs` 的 `CASCADE`、`check-dist.mjs` 的 `markers`、`ui/README.md` 表格）；本版没到需要拆文件的体量（现 397 行） |
| T8 | RPC 改名 `claude_sessions_*` → `sessions_*`，**旧名保留为别名一个版本**（3.6.0 删）；REST **只增不改**：新增 `/admin/api/sessions*`，旧 `/admin/api/claude-sessions*` 原样保留 | RPC 是控制台自己的传输层（前后端同版本发布），改名风险可控；REST 是 `docs/admin-api.md` 里对脚本承诺的机器接口，只能加不能改 |

---

## 1. 现状为什么不够（只列会被本次改动碰到的）

| # | 事实 | 证据 |
|---|---|---|
| P1 | 列表是「全量候选排序 → `skip/take` → 只读命中行」，**没有任何组的概念** | `src/claude_sessions/read.rs:11-16` |
| P2 | wire 不是结构体序列化，是**手拼 `json!`**；给 `ListPage` 加字段不会出现在网络上 | `src/admin/ops_sessions.rs:86-98`（`ListPage` 定义在 `types.rs:195-205`，从不直接被序列化） |
| P3 | 分组键今天只有一个候选且**不能只靠它**：目录名编码有损、同一 transcript 内 `cwd` 会变（实测 31 个里 2 个来回切） | `audit-3.5.6/backend-sources.md` §3；`types.rs:39-44` 明写不用目录名反推 `cwd` |
| P4 | 点击会话**不写地址**；`ui/src` 全库只有两处 hash 写入，都只写 tab 段 | `main.js:102`、`main.js:230`；`index.js:420-422` 只改 `state.selected` |
| P5 | 于是刷新必丢；而深链接进来后点另一行，hash 仍是旧 id → **刷新跳回旧会话**（反向分叉） | `audit-3.5.6/frontend-routing.md` §1.2 实测表 |
| P6 | 「来源」是文案级概念：`session_sources()` 是返回字面量的私有函数，`dsh_local.available:false` 是手写常量，没有任何代码探测 DSH 目录；`#sessions-sources` 按钮**没有 click 监听** | `ops_sessions.rs:9-19`；`list.js:39-50`；实测点击后调用增量 0 |
| P7 | `claude` 硬编码 **14 处**（路径解析、id 校验、`GATEWAY_PREFIX` 两份、source 描述符、REST 路径、RPC 名、中文错误子串判 404…） | `backend-sources.md` §2 全表 |
| P8 | 本机 31 个会话分布在 10 个有会话的项目目录（13 个目录里 3 个空），最大一组 10 个——**平铺列表的信息损失是真实的**，不是假想需求 | `audit` §3 实测 |

---

## 2. 契约 A：列表 wire（分组 + 来源）

### 2.1 请求参数

`ListArgs`（`types.rs:168-171`）与 REST 的 `ClaudeSessionsQuery`（`observe.rs:120-124`）**同步**新增，全部 `Option`：

| 参数 | 取值 | 语义 |
|---|---|---|
| `group_by` | `none`（缺省）或 `project` | `none` = **今天的行为，一个字节不变** |
| `group` | 组 key（不透明串，服务端生成） | 只在 `group_by=project` 时有效：过滤到该组，`offset`/`limit` 改在**组内**生效 |
| `group_limit` | 缺省 10，clamp `1..=200` | 每组返回多少条会话 |
| `source` | slug，如 `claude` | 只在有多个来源时有意义；3.5.6 只有 `claude`，传别的应报「无效的会话来源」（**用 `无效的` 开头**，否则 `observe.rs:68-75` 会把它变成 500） |
| `limit`/`offset` | 不变 | `group_by=none` 时语义完全不变 |

### 2.2 响应（在 3.5.5 的键上**只增不改**）

```jsonc
{
  "available": true, "reason": null, "root": "…", "sources": [ … ],   // 不变
  "stats_available": true, "gateway_error": null,
  "total": 31, "offset": 0, "limit": 50,                              // 语义不变（= 命中过滤的会话总数 / 组内切片）
  "group_by": "project",
  "groups": [
    { "key": "claude:-Users-denis-Sites-proxy-rs",
      "label": "proxy-rs",                      // 展示名，可由 cwd 末段得出
      "cwd": "/Users/denis/Sites/proxy-rs",     // 规范化绝对路径；未知时 ""
      "sources": ["claude"],                    // 该组出现过的来源 slug（去重、升序）
      "count": 10,                              // 该组会话数（stat，精确）
      "last_seen_ms": 1794332000000,            // 组内 max(file mtime)，精确
      "size_bytes": 8101888,                    // 组内 Σ size，精确
      "more": false,                            // count > sessions.len()
      "sessions": [ /* 与扁平模式同构的 SessionRow */ ] }
  ],
  "sessions": null                              // ⚠ 分组模式下为 null，见下
}
```

**为什么分组模式下 `sessions` 是 `null` 而不是把行拍平再抄一遍**：抄一遍会让 payload 翻倍，而分组模式只会被「发过 `group_by` 的客户端」看到——旧客户端永远不发这个参数，所以它永远见不到 `null`。这是本契约里唯一一处「旧字段在新模式下不出现」，必须写在契约里而不是靠读者猜。

每行新增一个**行级**字段（`SessionRow` 顶层，与 flatten 后的 summary 并列）：
`"source": "claude"`。它属于「谁给的」而不是「会话是什么」，所以放 row 不放 summary。

### 2.3 可验收不变量

| # | 不变量 | 怎么验 |
|---|---|---|
| I1 | 不带 `group_by`/`group`/`source` 时，响应**键集合**与 3.5.5 完全一致，`sessions` 数组、排序、`total`/`offset`/`limit` 语义不变 | 新增 Rust 测试直接断言键集合（**今天没有任何测试检查这些键**，`grep body\["total"\]` 在 `tests/admin_api.rs` 为空） |
| I2 | `group_by=project` 时 `Σ groups[].count == total`；`more == (count > sessions.len())` | Rust 测试 |
| I3 | 组按 `last_seen_ms` 降序（同值按 `key` 升序）；组内按 `(mtime desc, id desc)`，与扁平模式一致 | Rust 测试 |
| I4 | `group=<key>` 时 `groups.len()==1`，`total` = 该组条数，`offset`/`limit` 在组内切 | Rust 测试 |
| I5 | 每行 `source` 恒等于该行来源 slug，且 `gateway.session_id` 以 `<source>:` 开头（今天两份 `format!("claude:{}")` 是硬编码重复，见 P7 #6/#10） | Rust 测试 + 现有 `gateway_join_uses_the_claude_namespace` 保留 |
| I6 | `limit`/`group_limit` 均 clamp `1..=200`，`limit=0` → 1（沿用 `read.rs:9`） | 现有行为 + 新参数各一条 |

### 2.4 组 key 与组标签的生成规则（必须显式定，因为它不是单值）

1. **身份**：`key = "<source_slug>:" + <该来源自己的项目键>`。Claude 的项目键就是目录名 `encoded_project`（文件系统内唯一）。key 对客户端**不透明**，只用来回传 `group=<key>`。
2. **`cwd`**：取该组**最新那个会话**转写文件头部出现的第一条 `cwd`（每组上限读 64 KiB，**不读全文**）。本机实测：10/10 组命中，10 组总共读 40 KiB（4 KiB 上限）到 640 KiB（64 KiB 上限），耗时 0.3–2.1 ms；取不到就 `""`。之所以不用「组内所有文件都扫一遍」：那会把列表成本变 `O(total 字节)`。
3. **`label`**：`cwd` 的末段；`cwd` 为空时用目录名去掉前导 `-` 兜底，并**只在 `title` 里给全名**（目录名解码有歧义，绝不能当成路径显示）。
4. **不做的事**：同一 transcript 内 `cwd` 中途变化（实测 2 例）**不**拆组；跨来源同项目合并**本版不做**，只保证 `cwd` 已在 wire 上、合并规则留到接第二个来源那一版（判定与成本见 `backend-sources.md` §3）。

### 2.5 界面（这一版实际长什么样）

侧栏仍 260px（`.sess-split` 的 `grid-template-columns`，`view-sessions.css:10-17`），组头不占 `.sess-row` 类（否则 60px 行高契约与 `data-id` 语义都破）：

```
┌─ 会话 ─────────────────────────────────────────────────────────┐
│ [Claude Code] [DSH]             共 31 个 · 10 个项目            │  ← 工具栏：来源筛选（DSH 仍 disabled）
├────────────────────────┬──────────────────────────────────────┤
│ ▾ proxy-rs        12   │                                      │  ← .sess-group-head（吸顶）
│   ┃ 规划3.5.6…  2h 前  │                                      │  ← 组内行：.sess-row-wrap 追加 .is-nested
│   ┃ 修前端开发…  8h 前  │                                      │
│   ┃ …（共显示 10 行）   │                                      │
│   ┗ 显示更多 2 个 (12)  │                                      │  ← .sess-group-more：count > group_limit 时
│ ▸ deepseek-harness 4   │           （未选中时的空态）           │
│ ▾ -private-tmp     8   │                                      │
│   ┃ …                  │                                      │
├────────────────────────┴──────────────────────────────────────┤
│                              （分组模式不显示分页器）           │  ← .sess-pager 隐藏，见下表
└────────────────────────────────────────────────────────────────┘
```

> 图里的 `12` 是示意（本机真实数据最大一组是 10，刚好等于默认 `group_limit=10`，所以本机不会出现「显示更多」；合成 300 文件 fixture 会出现）。

| 元素 | 类名 | 规则 |
|---|---|---|
| 组头 | `.sess-group-head` | 高 28px（不占 60px 行高池），与 `.sess-row` 同级放进 `.sess-rows`；左侧 `▾/▸`、中间 `label`（`title`= 完整 `cwd`）、右侧 `count`；点击折叠/展开 |
| 组头第二行 | `.sess-group-meta` | 「2 小时前 · 7.8 MB」，来自 `last_seen_ms` / `size_bytes`；用与 `.sess-row-foot` 相同的 `formatLocalDateTime`/`formatBytes` |
| 组内行 | `.sess-row-wrap.is-nested` | 只加 `padding-left: 14px` 与左侧 2px 竖线；**行高、`.sess-row-title` 的 `padding-right: 26px` 全部不动**（`view-sessions.css:119-121` 的注释明写内缩只能加在 title 上，组内缩进同理只能加在 wrapper 上） |
| 「显示更多」 | `.sess-group-more` | 一行文本按钮。点击 → `group=<key>&group_limit=<count>`（≤200）只取该组，把返回的行**就地拼进该组**，不重画整页、不滚动到顶 |
| 空组 | — | `count==0` 的组**不渲染**（本机 13 个目录里 3 个是空的，渲染出来只是噪音） |
| 折叠状态 | module 内 `Set<key>` | T6：只存内存；默认全展开；深链接进入时**强制展开**该会话所在组并 `scrollIntoView` |
| 分页器 | `.sess-pager` | 分组模式下**隐藏**（`group_by=project` 时每个组自带「显示更多」）；扁平模式保持现状。两个分页维度同时存在是「同一屏两套翻页」的经典坑，本版直接避开 |

**前端发什么**：会话页固定发 `group_by=project&group_limit=10`（3.5.6 的产品形态就是二级列表）；工具栏「刷新」原样重发。扁平模式（`group_by` 缺省）只有 **REST 脚本调用方**会遇到，前端不用它——但契约里必须保留，否则「老脚本行为不变」这条就不成立了。

**`renderList` 的退化规则（重要，防 mock 与旧后端）**：收到响应但**没有** `groups` 字段时，把 `sessions` 当作「一个匿名组」渲染成扁平列表（即 3.5.5 的样子）。这样 `ui/tools/smoke` 的 mock、以及「页面缓存里还是旧 JS」这种滚动发布瞬间都不会白屏。函数签名从
`renderList(page, { selected, empty })`
改为
`renderList(page, { selected, empty, collapsed })`
——多一个折叠集合，其余不变。


---

## 3. 契约 B：地址与持久化

### 3.1 地址形态

| 形态 | 含义 | 谁生成 |
|---|---|---|
| `#sessions` | 会话 tab，不指定会话 | tab 点击、`switchTab` |
| `#sessions/<uuid>` | **旧形态，恒等于 `claude`** | 3.5.5 已发出的链接、用户收藏 |
| `#sessions/<source>/<id>` | 新形态 | 点击行（`replaceState`）、`copyDeepLink`、`open_session_in_browser` |

`<source>` 是**枚举 token**（`[a-z][a-z0-9_]{0,15}` 且必须能在注册表里查到），`<id>` 由**该来源自己的**校验器判定。所以登录白名单那句论证继续成立：输出恒为 `/admin/#sessions/<已枚举来源>/<已校验 id>`，无法表达 scheme、host 或路径穿越。

`tests/session_open_url.rs:249-273` 现有的 10 条必拒形态**全部继续拒**，三段形态须补三类新反例：来源不存在（`sessions/nope/<uuid>`）、来源合法但 id 不合法（`sessions/claude/not-a-uuid`）、以及**来源段本身携带分隔符/大写**（`sessions/../<uuid>`、`sessions/CLAUDE/<uuid>`）。

**旧形态永远解析成 `claude`**，这条必须写死：Codex 的 id 也是 UUID（实测 13/13 能过现有校验），若让旧形态自己去猜来源，同一个 UUID 可能落到两个来源。历史包袱在这里反而是确定的规则。

### 3.2 谁在什么时候写什么

| 动作 | URL | localStorage |
|---|---|---|
| 点击一行并**成功**打开 | `replaceState` → `#sessions/<source>/<id>` | 写 `{source, id, at}` |
| 深链接 / 刷新进入 | 不动（用户给的地址不改写） | 同步写（打开成功后） |
| 切到别的 tab | 变 `#logs`（`switchTab` 原逻辑不变） | 不动 |
| 切回会话 tab（URL 无会话） | **不动**，仍是 `#sessions` | 读 → 静默恢复 |
| 恢复失败（文件已删/来源下线） | 不动 | **清键**，`renderDetailMessage` 静默说明，**不弹 toast** |

两条硬规则：

- **URL 优先于 storage。** 深链接/收藏/复制出来的地址指向什么就显示什么；storage 只在 URL 没指定会话时兜底。
- **切 tab 回来不把 id 写回 URL。** 这是刻意让 `smoke` 的 `pane sessions: routed to #sessions`（`scenarios-pages.mjs:89`，要求 hash **精确**等于 `#sessions`）保持绿，也是让「刷新」只有一条路径：URL 有 id 就用 URL，没有就用 storage。若把 id 写回 URL，这条断言会被无意打红，而它会红得很晚。

### 3.2.1 恢复逻辑插在哪（这一处容易写错）

`applyTab('sessions')` 现在是 `Promise.resolve(loadSessions()).then(consumeDeepLink, consumeDeepLink)`（`main.js:127`）。而 `loadSessions` 在 `loadedOnce` 之后**早退返回 `undefined`**（`index.js:386`），所以「切走再回来」时这个 `.then` 仍会跑。

恢复**不能**塞进 `consumeDeepLink`：那个函数语义是「把 hash 里的会话应用上」，让它去读 storage 会让两个真相源混在一个函数里，`hashchange` 分支（`main.js:144-148`）也会跟着误触发。

正确形状是加一个同级的 `restoreLastSession()`，并让它在 `consumeDeepLink` **之后**跑：

```js
Promise.resolve(loadSessions())
  .then(consumeDeepLink, consumeDeepLink)     // URL 优先：有深链接就选中它
  .then(restoreLastSession);                  // 没有就退到 storage
```

`restoreLastSession()` 自己在开头判断 `if (deepLinkSessionId() || state.selected) return;` —— 于是「URL 优先」「已选中不重选」两条都由一行守卫表达，而不是靠调用顺序的巧合。

### 3.2.2 点击一行的顺序（有两处必须按这个次序，否则会出现「刷新回到旧会话」的同类 bug）

`selectSession(id)`（`index.js:420-441`）现在第一行就 `state.selected = id`，然后才 `await invoke(...)`。改造后：

```
1. state.selected = id; renderList(...)        ← 既有：立即高亮，不等网络
2. await invoke('sessions_detail', {source, id}) ← 既有
3. 成功：replaceState('#sessions/<source>/<id>') + prefs.set({source,id,at})
   失败：不动 URL、不动 prefs（保持既有 renderDetailMessage + toast）
```

**为什么写 URL 必须在成功之后**：反过来写的话，一个读不出来的会话（文件被删、来源下线）会被写进地址栏与 storage，于是刷新一次失败一次、每次都要看同一个错误——把一个瞬时故障变成「这个页面坏了」。今天的 `selectSession` 已经把「失败」当成一等结果处理（`renderDetailMessage` + toast），这里只是让 URL 与 storage 跟随同一个判定，不多一条逻辑。

**为什么 `replaceState` 不会打环**：`history.replaceState` **不触发 `hashchange`**（只有 `location.hash =` / 导航才触发）。所以写地址不会回调到 `consumeDeepLink`，不需要任何重入守卫。这一点要在代码注释里写明，否则下一个人会加一个多余的 `if (writing) return;` 标志。

### 3.3 存储契约

新建 `ui/src/core/prefs.js`（前端第一个 Web Storage 模块，**新约定，要登记进 `ui/README.md`**）：

```js
// 键名带版本；读写全部 try/catch，失败降级为内存 Map —— 隐私模式/禁用存储不得让页面崩。
const KEY = (n) => `proxy-rs.${n}.v1`;   // 3.5.6 只用 lastSession
```

- `proxy-rs.lastSession.v1` = `{ "source": "claude", "id": "<uuid>", "at": <epoch ms> }`
- 不设 TTL。理由：过期与否的判据是「文件还在不在」，而那是 `sessions_detail` 的返回值，前端不必自己猜一遍。
- `localStorage` 不可用 → 全程走内存，行为退化为「刷新丢会话」（与 3.5.5 相同，不是新故障）。
- 多标签页：本版**不做** `storage` 事件同步；两个标签页各自为政是已知限制，写进 README 而不是假装一致。

### 3.4 链接形状被 7 处独立钉死（漏一处就是静默丢链）

前置事实：`#sessions/<uuid>` 这个形状**不是**一处定义的，它被 7 个互相不知道对方存在的地方各自校验/生成（`frontend-routing.md` §2.3）。改形状必须逐个过一遍：

| # | 位置 | 角色 | 3.5.6 要做什么 |
|---|---|---|---|
| 1 | `ui/src/main.js:81-84` `tabFromHash()` | 只读首段、`toLowerCase()` | **不动**（首段仍是 `sessions`；注释已明写 id 段必须原样透传） |
| 2 | `ui/src/views/sessions/index.js:453-458` `deepLinkSessionId()` + `:50` `SESSION_ID_RE` | 解析 id | 返回 `{source, id}`；2 段→`claude`，3 段→查来源 + 该来源 id，其余→`null` |
| 3 | `ui/server/login.js:49-70` `LANDING` | 登录页白名单 | 同时认三段形态（来源枚举 + id 正则）；**不改会让登录往返静默丢链** |
| 4 | `src/admin_routes/login.rs:76-99` `next_hash` | 服务端白名单（真边界） | 同上；输出恒 `/admin/#…`，开放重定向论证不变 |
| 5 | `src/desktop_hooks.rs:162-172` `session_console_url` + `:182-201` `is_console_url` | 生成 + 复验浏览器要打开的地址 | 生成带 `source`；`is_console_url` **不动**（它不看 hash） |
| 6 | `src-tauri/src/main.rs:211` `open_url_in_browser` | 交给 OS 前的第三道 | **不动**（同样只验到 `/admin/` 前缀） |
| 7 | `ui/src/views/sessions/list.js:197-199` `sessionDeepLink()` | 复制链接 | 产三段形态；对应 smoke `scenarios-views.mjs:187-192` 的断言同步 |

外加两笔**已经发出去的**形状：`open_session_in_browser` 生成的 `http://127.0.0.1:<port>/admin/#sessions/<uuid>`（旧客户端、旧窗口、用户收藏里都有）与 `ui/server/bridge.js:83-84` / `ui/src/core/ipc.js:115,118` 把当前 hash 原样带到登录页。前者靠「旧形态恒等于 `claude`」兼容，后者靠第 3/4 两处同时放宽。

`is_console_url` 只校验到 `/admin/` 前缀、**不看 hash**——这一点要写进代码注释，否则下一个人会以为它在管 hash，从而在新增来源段时误以为「这里可能有漏洞」而做出没必要的改动。

---

## 4. 契约 C：来源抽象（只做接缝，不接第二个源）

### 4.1 形状

新增模块 `src/sessions/`（`claude_sessions` **整体不动**，它现在的 1,930 行（6 个文件）与 17 个 `#[test]` 原样保留，作为 Claude 的实现）：

```
src/sessions/mod.rs      trait SessionSource + registry() + 分组装配（来源无关）
src/sessions/claude.rs   薄适配层：把现有 claude_sessions 的能力包成 SessionSource（不搬代码）
```

```rust
pub trait SessionSource: Send + Sync {
    fn slug(&self) -> &'static str;            // "claude" —— 进地址、进 ListArgs.source
    fn kind(&self) -> &'static str;            // "claude_local" —— 既有 wire 的 data-kind，冻结
    fn label(&self) -> &'static str;           // "Claude Code"
    fn gateway_prefix(&self) -> &'static str;  // "claude:" —— 查 stats.db 的命名空间
    fn is_valid_id(&self, id: &str) -> bool;
    fn root(&self) -> Option<PathBuf>;                     // None = 本机没有这个来源的数据
    fn scan(&self) -> Result<Vec<Candidate>>;              // 一次 depth-1 walk，同步阻塞
    fn summary(&self, c: &Candidate) -> Result<SessionSummary>;
    fn read(&self, id: &str, args: &DetailArgs) -> Result<SessionDetail>;
    fn read_subagent(&self, id: &str, agent: &str, args: &DetailArgs) -> Result<SessionDetail>; // 默认 Err("该来源无子代理概念")
}

pub fn registry() -> &'static [&'static dyn SessionSource];   // 3.5.6 只有 claude 一项
```

**trait 里没有 `available`/`note`**：注册进 `registry()` 就意味着「能读」，不可用的来源根本不在里面（见下面第 4 条）。这样「可用性」只有一处真相，不会出现「注册了但 describe 说不可用」的矛盾状态。

四条设计约束（每条都能对着审计里的数字讲）：

1. **`scan()` 同步、一次一来源**——保持 `claude_sessions.rs:17-23` 的「整个请求只 `spawn_blocking` 一次」。将来接 DSH 时，它的 `scan()` 是「读 projcache 的 389 个 JSON = 53 ms」还是「解压首行 = 936 ms」是它自己的事，不改变这里的形状。
2. **`Candidate` 是 `pub(crate)`**（`types.rs:229-234`），改它不破坏 wire，是**最便宜的注入点**。本次给它加 `size_bytes` 与 `source`（stat 阶段顺手拿，零新 IO）。
3. **`summarize.rs` / `parse.rs` 一律不进 trait**。它们是 Claude 的行格式解析，DSH 的 `{type,seq,time,data}` 事件流与 Codex 的 `session_meta` 一个都用不上（`backend-sources.md` §5）。把解析塞进 trait 只会让「新来源」变成「改中心文件」。
4. **`session_sources()` 不再是字面量**，而是由「注册表 + 已知未实现表」派生。这里必须区分两件事，否则 K3 会被无意打破：
   - `registry()` = **能读的来源**，3.5.6 只有 `claude`（`available:true`）；
   - `PLANNED` = **知道的、还不能读的来源**，`&[("dsh", "DSH", "dsh_local", "DSH 会话是 zstd 压缩的本地存档，暂不支持浏览")]`（`available:false` + `note`）。

   两者拼起来才是 `sources` 数组，顺序仍按 `[claude, dsh]`——于是 `admin_api.rs:1026-1029` 与 `scenarios-views.mjs:118` **原样通过**，而「DSH 是灰色带原因」这条产品行为一个字都没变。将来真接 DSH 时，动作是「把它从 `PLANNED` 挪进 `registry()`」，不是「在 `json!` 里改一个字面量」——这才是这次抽象要买到的东西。

### 4.2 证明接缝是真的：一个测试专用的来源

3.5.6 不接真来源（D3），所以「扩展性」不能靠嘴说。用 `#[cfg(test)]` 的 `FakeSource`（两个会话、一个项目、无子代理）跑通 `registry → scan → group → wire`，断言：`sessions[]` 出现两行、`source == "fake"`、`groups` 只有一组、`groups[].count == 2`。**这比接一个半成品的 DSH 读取器更能证明接缝**，且零发布风险。

配套一条「注册表契约测试」（这是接缝的第二半，容易被忘）：

| 断言 | 防的是 |
|---|---|
| 每个 `registry()` 成员的 `slug` 互不相同、且匹配 `^[a-z][a-z0-9_]{0,15}$` | 地址段冲突 |
| `slug` 集合与 `PLANNED` 的 slug 集合**无交集** | 同一个来源同时「能读」和「不可读」 |
| `gateway_prefix` 不重复 | stats.db 命名空间串台（今天 `claude:` 是唯一的） |
| `sources` 数组里 `kind` 不重复、且顺序稳定 | 破坏 `admin_api.rs:1026-1029` 的顺序断言 |

### 4.3 两个标识符不能混：`kind` 与 `slug`

现有 wire 里来源叫 **`kind`**，取值 `claude_local` / `dsh_local`（`ops_sessions.rs:9-19`，且 `admin_api.rs:1026-1029` 与 `scenarios-views.mjs:118` 把它钉死了）。地址里需要一个短、稳定、能进 URL 的标识——`claude_local` 进地址是噪音。

所以定两个字段、**各管一件事**：

| 字段 | 取值 | 用途 | 会不会变 |
|---|---|---|---|
| `kind` | `claude_local` / `dsh_local` / `codex_local` | 现有前端 `data-kind` 分段、现有测试 | **不变**（K3） |
| `slug` | `claude` / `dsh` / `codex` | 地址段、`ListArgs.source`、行级 `source` 字段、`gateway_prefix` 前缀 | 一旦发布就冻结 |

`slug` 只允许 `[a-z][a-z0-9_]{0,15}`，且**必须在注册表里存在**——这是「地址段不能成为自由字符串」的具体落地。三处校验（JS 正则、`login.js` 的 LANDING、`login.rs` 的 `next_hash`）都靠「先查枚举再校验 id」，而不是靠字符集猜。

### 4.4 详情命令怎么带来源

`claude_sessions_detail` → `sessions_detail`，入参 `{ "source": "claude", "id": "…", "args": {…} }`：

- `source` **缺省 = `claude`**，所以旧调用方（含 `ui/tools/admin-smoke.mjs:371` 与一切外部脚本）不动；
- 未知 `source` → `bail!("无效的会话来源: {source}")`（**「无效的」前缀是硬要求**，否则 `observe.rs:68-75` 把它变成 500 而不是 404）；
- 后端按 `source` 取自注册表 → `is_valid_id` → `read()`，**先校验来源再校验 id**，两步都在拼路径之前。

「先校验再拼路径」这条在 `paths.rs:42-44`（严格 UUID）与 `:98-104`（canonicalize + `starts_with`）是 Claude 的实现细节，多来源后必须由 trait 的每个实现各自保证；`Candidate` 里带 `source` 也让「一个 id 属于哪个来源」在装配层就定死，不会在读的时候再猜。

---

## 5. 分阶段实施（每阶段独立提交、独立验证）

| 阶段 | 内容 | 关键文件 | 验收 |
|---|---|---|---|
| **A. 后端分组** | `Candidate` + `size_bytes`/`source`；stat 聚合出 `groups`；`group_by`/`group`/`group_limit`/`source` 四参数；组 `cwd` 探测（每组读最新文件头 ≤64 KiB）；wire 在 `json!` 里补齐 | `claude_sessions/{types,paths,read,summarize}.rs`、`admin/ops_sessions.rs`、`admin_routes/observe.rs` | I1–I6 六条不变量各一个 Rust 测试；`cargo test`、`clippy -D warnings` |
| **B. 来源接缝** | 新建 `src/sessions/{mod,claude}.rs`；`session_sources()` 由 `registry()` + `PLANNED` 派生；`admin_rpc` 改 `sessions_*` + 旧名别名；REST 加 `/admin/api/sessions*`（旧路径保留）；`desktop_hooks::session_console_url` 带 source 参数 | 上表 + `sessions/{mod,claude}.rs`、`admin_rpc.rs`、`admin_routes/mod.rs`、`desktop_hooks.rs` | `FakeSource` 端到端测试 + 注册表契约测试（§4.2）；现有 4 个 `admin_api.rs` 会话测试**必须原样通过**（`sources[]` 顺序与内容不变）；`ui/tools/admin-smoke.mjs:371` 仍绿 |
| **C. 前端二级列表** | 组头渲染（`.sess-group-head`，**不带** `.sess-row` 类）、组内行缩进、「显示更多 N 个」、来源筛选接线；`renderList` 收 `groups`，响应无 `groups` 时退化成单组（防 mock/旧后端）；样式追加进 `view-sessions.css` | `views/sessions/{list,index}.js`、`style/view-sessions.css`、`tools/smoke/{fixtures,scenarios-views,scenarios-states}.mjs` | `check:ui`、`check:dist`、`npm run smoke` 全绿；新增 `checkSessionsGrouping` 断言组头数/组内行数/`more` 行为；`scenarios-states.mjs:89-97` 的空列表 override 要补 `group_by`/`groups` |
| **D. 地址 + 持久化**（前端/后端同一提交，因为链接形状是横跨两边的契约） | `deepLinkSessionId` 返回 `{source,id}`；点击写 `replaceState`；`core/prefs.js` + 恢复流程；`sessionDeepLink` 产新形态；`login.js` LANDING、`login.rs` `next_hash`、`desktop_hooks` 同步；旧形态兼容 | `views/sessions/{index,list}.js`、`core/prefs.js`、`server/login.js`、`admin_routes/login.rs`、`desktop_hooks.rs`、`tests/session_open_url.rs` | 新增 smoke `checkSessionsPersistence`（点行→reload→仍是它；点行→切 logs→回 sessions→仍是它；深链接优先于 storage；坏 id 静默无 toast）；`session_open_url.rs` 新旧形态各一组 |
| **E. 文档与发布** | `README.md`（会话页能力）、`docs/admin-api.md`（§本地会话：新参数、新路径、`groups` 形状、RPC 改名与别名）、`ui/README.md`（§5 地址表 + storage 登记 + 二级列表）；`docs/release-3.5.6.md`；版本号 **8 处声明 / 6 个文件**同步（`tests/release_consistency.rs:161-207` 会逐个点名） | 文档 + 6 个版本声明文件 | `cargo test --test release_consistency`；`cargo metadata --locked` |

依赖：C 依赖 A；D 依赖 B（要用 `source` slug 与注册表）；E 最后。A∥B 都动后端但**文件基本不重叠**（A 在 `claude_sessions/`，B 在 `sessions/` + 命名），仍建议顺序做，因为 B 要复用 A 加在 `Candidate` 上的字段。

---

## 6. 必须保持不变（回归网，改动前先读这一节）

| # | 必须保持 | 守卫 |
|---|---|---|
| K1 | `#sessions/<uuid>` 仍能被解析并选中 | `scenarios-views.mjs:255-258`、`session_open_url.rs:228-247` |
| K2 | `open_session_in_browser` **只收 id、URL 由后端拼**，客户端传的 `url/host/port` 被忽略 | `session_open_url.rs:415-441`（`the_rpc_ignores_client_supplied_url_host_and_port`）+ `:543-575`（`the_launcher_only_ever_receives_the_checked_shape`） |
| K3 | `sources[0].kind=="claude_local"`、`sources[1].kind=="dsh_local"` 且 `available==false`、`note` 非空 | `admin_api.rs:1026-1029`、`scenarios-views.mjs:118` |
| K4 | gateway join 键 `claude:<uuid>`；未记录的会话 `gateway` 是 `null` 不是 0 | `gateway_join_uses_the_claude_namespace` |
| K5 | 不带新参数时列表行为字节级不变 | 新增 I1 测试（**今天没有**） |
| K6 | 空列表文案含「没有找到」、且不伪造任何行 | `scenarios-states.mjs:124-125` |
| K7 | 时间线不出现内层滚动条（工具输出折叠而非滚动） | `scenarios-views.mjs:122` |
| K8 | 每行恰 1 个 `.sess-row-open`，且点它不触发选中 | `scenarios-views.mjs:128-132` |

**要主动改的断言**（改了就说明我们确实动了行为，不是回归）：

| 断言 | 现状 | C/D 阶段之后 |
|---|---|---|
| `scenarios-views.mjs:115` `sessions list renders one row` | `.sess-row` = 1 | 仍 = 1，**前提是** fixture 的 `groups` 里只有 1 组 1 行——组头不占 `.sess-row`，所以这个数不需要改 |
| `scenarios-views.mjs:187-192` `the copied link is this console's deep link` | 后缀 `/admin/#sessions/<uuid>` | `/admin/#sessions/claude/<uuid>` |
| `fixtures.mjs:78-107` `claude_sessions_list` mock | 无 `groups`/`source` | 补 `source`（每行）、`group_by`、`groups`（1 组 1 行）；**另加**一个「无 `groups` 字段」的 override 场景验证退化路径 |
| `fixtures.mjs:146` `open_session_in_browser.url` | `#sessions/<uuid>` | `#sessions/claude/<uuid>` |

`fixtures.mjs` 是「mock 与真实必须同形」的地方。`ui/README.md:134` 已经把这条写成了规则：**「若断言值确实需要变，请一并更新脚本里的 `EXPECTED`，让改动是显式的而不是被忽略的」**；`ui/README.md:184-186` 则记录了旧版 dev mock「总会和 `src/claude_sessions.rs` 差一个字段」的教训。漏改 fixture 的后果不是失败，而是**mock 与真实偏离**，之后所有断言都在一个不存在的形状上绿着。所以 C 阶段改 wire 字段时，同一提交里必须改 fixture。

**不改也仍然绿的一条**（靠别名，不是靠改测试）：`admin-smoke.mjs:371` 调的是 `claude_sessions_list`。B 阶段保留旧名别名就是为了让这条断言继续绿——如果它被迫改，说明「旧调用方不动」这个承诺已经破了，要当成设计缺陷处理，不是「顺手改一下测试」。

**最容易无意打红的一条**：`scenarios-pages.mjs:89`（`location.hash === '#sessions'`）。任何「进入 tab 就自动把 id 写回 URL」的实现都会打红它——这正是 §3.2 那条「切 tab 回来不写 URL」的由来。

---

## 7. 风险与缓解

| # | 风险 | 缓解 |
|---|---|---|
| R1 | **`ListPage` 加字段却不改 `ops_sessions.rs` 的 `json!`** → 字段在网络上根本不存在，而**没有任何测试会红**（本版最危险的静默失败） | A 阶段第一条测试就断言响应键集合；B 阶段把 `json!` 挪进一个 `fn page_json(...)` 并加「结构体字段 ⊆ JSON 键」的对照断言 |
| R2 | 分组后读文件数上限从 `limit` 变成 `组数×group_limit`（合成 300 文件/24 组 = 240 次，≈77 ms） | `group_limit` 缺省 10 且 clamp ≤200；`spawn_blocking` 仍只有一次；本机真实数据 `group_limit=10` 读 31 个文件、与今天**完全相同**，把它写进注释作为容量依据 |
| R3 | `cwd` 探测给每个组多开一次文件读 | 只读每组**最新**一个文件的头部 ≤64 KiB（实测 10 组 2 ms）；探测失败不报错，`cwd: ""` |
| R4 | 链接形状被 7 处独立钉死（§3.4），改漏一处 = 静默丢链（登录往返最典型） | D 阶段照着 §3.4 的表逐行过：2/3/4/5/7 要改，1/6 明确不动；并在 `session_open_url.rs` 加「三段形态也必须落回 `/admin/#…`」的正向 + 反向测试 |
| R5 | 点击写 URL 用 `hash =` 会给每次点击塞一条 history | 统一 `replaceState`（T3），并加一条 smoke 断言「点 3 个会话后 `history.length` 不变」 |
| R6 | storage 恢复出一个已删除的会话 → 每次进页面弹一个错误 toast | 恢复路径走 `silent` 分支：失败只清键 + 空态说明，不 toast（§3.2） |
| R7 | `dsh_local` 被误当成「已接入」→ `smoke` 打红、产品行为漂移 | 本版**不改** DSH 的 `available`：它留在 `PLANNED` 表（§4.1 第 4 条），只有 `registry()` 里的来源才 `available:true`；`smoke` 与 `admin_api` 断言保持绿 |
| R8 | 隐私模式/禁 cookie 环境下 `localStorage` 抛异常 | `prefs.js` 全部 try/catch + 内存降级；smoke 加一条「`localStorage` 被禁时页面仍渲染」的场景 |

---

## 8. 明确不做（写下来防止范围蔓延）

1. **不接 DSH / Codex**。真接 DSH 需要新增 `zstd`（`Cargo.toml`/`Cargo.lock` 实测均无）＝ 往纯 Rust 构建链插 C 库，且 520 MiB 压缩 / 1820 MiB 解压 / 全量解压 2.99 s，与「一个请求一次 `spawn_blocking`」的前提冲突。Codex 还要额外解决「目录按日期分层、分组键必须解压首行、双 uuid 陷阱」。这些都在 `backend-sources.md` §5，留给下一版。
2. **不做跨来源同项目合并**。契约里留了 `cwd`，但合并规则（取哪个 `cwd`、目录名编码碰撞时怎么办）没有真实数据可验证，本机 31 个会话**零冲突**、揭示不了规则，硬定就是编。
3. **不做会话行级 token 汇总到组头**。组头只给 stat 能证明的三个数字（会话数、最近活动、字节量）；turns/tokens 要给全量读文件，会把列表成本变成 `O(total)`。
4. **不做折叠状态持久化**、**不做多标签页 storage 同步**、**不做会话搜索/过滤（除来源外）**。
5. **不改 `is_console_url`**（它不看 hash，改它不解决任何问题）。`open_session_in_browser` 只加「来源」入参并校验，仍然只收 id + source，URL 仍由后端拼。

---

## 9. 工作量与顺序

| 阶段 | 估量（含测试与注释） | 可并行 |
|---|---|---|
| A 后端分组 | ~350 行 Rust + ~150 行测试 | 与 C 的前置无冲突，但 C 要等 A |
| B 来源接缝 | ~250 行 Rust + ~120 行测试 | 依赖 A 的 `Candidate` 字段 |
| C 前端二级列表 | ~200 行 JS + ~60 行 CSS + smoke | 依赖 A |
| D 地址 + 持久化 | ~150 行 JS/Rust + ~120 行测试 | 依赖 B |
| E 文档发布 | 5 份文档 + 8 处版本声明（6 个文件） | 最后 |

合计约 **950 行生产代码 + 510 行测试 + 5 份文档**。按本仓库既往节奏（3.5.4 一个功能 ≈ 2 个提交、3.4.1 一次重构 17 个提交），预计 **6–8 个提交**。

一句话验收：**「打开会话页 → 看到按项目分组的两级列表 → 点一个会话 → 刷新还在 → 切到日志再回来还在 → 把地址发给别人，别人打开是同一个会话 → DSH 那条仍是灰色带原因」**，七步全过即完成。
