# proxy-rs v3.5.6 发布说明：会话列表二级聚合 + 会话地址可持久 + 来源可扩展

> 版本：**3.5.6**（上一版 3.5.5）
> 三项面向用户的改动（二级列表、地址可持久、来源可扩展），一项**零行为变更**的内部重构
> （Claude 专用代码让位给来源注册表），以及一处被结构守卫抓出来的真实依赖环。

---

## 1. 会话列表：按项目目录聚合成二级

会话页左侧原来是一条平铺的文件清单——31 个会话、10 个项目，全都挤在一列里，看不出哪些属于
同一个工程。现在它按**项目目录**聚合成两级：

```
▾ proxy-rs        10         ← 组头：项目名 · 会话数（整组精确）
  ⎿ 规划3.5.6…     2h 前
  ⎿ 修前端开发…    8h 前
  ⎿ …（默认显示 10 条）
▸ deepseek-harness 4
▸ -private-tmp     8
```

### 组头的三个数字来自 `stat`，不是猜的

`count` / `last_seen_ms` / `size_bytes` 都是对**整组**精确的，代价是零额外 I/O——目录扫描时
那一次 `stat` 顺手就拿全了（`Candidate` 加了个 `size_bytes` 字段，`paths.rs` 把已读到的
`meta.len()` 塞进去）。

这一点决定了实现方式：如果组头要显示「10 个会话 · 2 小时前」而对全量算一遍 `turns`，就必须
打开每一个文件，列表成本从 `O(limit)` 变成 `O(total)`。所以组头只给 stat 能证明的三件事，
行级的 `turns` / `tool_calls` 仍只对命中行算（沿用 3.5.5 的既有原则）。

### 默认 10 条是量出来的

`group_limit` 缺省 10，不是拍脑袋：本机组大小是 `[10, 8, 4, 2, 2, 1, 1, 1, 1, 1]`，
`group_limit=10` 时打开 **31** 个文件——与今天平铺 `limit=50` 打开的数量**完全相同**。
合成 300 文件 / 24 组的 fixture 是 240 个，按实测 96 ms 外推约 77 ms，且那是上限。

超出部分折叠成「显示更多 N 个」，点一下**就地拼进该组**，不重画整页、不滚动到顶。

### 分组键不能直接用目录名

这是本次最花时间的一个判断。Claude Code 把 `cwd` 的 `/` 编码成 `-` 当目录名：

```
/Users/denis/Sites/dolt2qlib/workspace/rd-agent  →  -Users-denis-Sites-dolt2qlib-workspace-rd-agent
/Users/denis/Sites/dolt2qlib-workspace-rd-agent  →  -Users-denis-Sites-dolt2qlib-workspace-rd-agent
```

两者同名。本机这个模糊目录**真实存在**（只是恰好只有一侧有数据）。更硬的是：**同一个会话
文件内的 `cwd` 会变**——实测 31 个可全读的转写里 2 个中途切换（一个在
`…/quant-temperature` 与 `…/quant-temperature/server` 之间来回 7 次）。

所以「会话属于哪个项目」本身不是单值。本版的处理是：

- **身份**用目录名（文件系统内唯一，stat 阶段零成本），组 key 为 `<source>:<目录名>`；
- **展示名**取该组**最新那个会话**转写文件头部（上限 64 KiB）的第一条 `cwd` 的末段；
  实测 10/10 组命中，10 组共读 40–640 KiB、耗时 0.3–2.1 ms。取不到就回落目录名去掉前导
  `-`，但那只是一个**名字**，绝不伪造路径；
- `cwd` 同时挂在 wire 上（`groups[].cwd`），供将来跨来源合并用——本版**不做**合并，因为
  合并规则没有真实数据可验证（本机 31 个会话零冲突，揭示不了规则），硬定就是编。

---

## 2. 会话地址：点了就记住，刷新/收藏/分享都回到同一个

3.5.5 的会话页有一个不容易被发现的问题：**点击一行不写地址**。全 `ui/src` 只有两处 hash
写入，都在 `main.js` 且只写 tab 段。后果有两个：

- 从 `#sessions` 进来点一个会话，刷新后**高亮与详情全丢**；
- 更糟的反向分叉：从深链接 `<A>` 进来后点行 `<B>`，hash 仍是 `<A>`，**刷新跳回 A**。

现在点击会写地址，用 `history.replaceState`（不产生历史条目；它不触发 `hashchange`，
所以没有重入需要防）。

### 两种形态，旧的永远能用

| 形态 | 含义 |
|---|---|
| `#sessions/<uuid>` | **3.5.5 形态，恒等于默认来源**（老链接、收藏、已发出的分享链接） |
| `#sessions/<source>/<id>` | 3.5.6 形态，来源显式 |

`<source>` 必须是注册表里的 slug（`[a-z][a-z0-9_]{0,15}`），`<id>` 由**该来源自己的**校验器
判定。于是「登录白名单无法表达 scheme / host / traversal」那个论证继续成立。

旧形态**写死**解析成 `claude`：Codex 的 id 也是 UUID（实测 13/13 能过现有校验），若让旧形态
自己去猜来源，同一个 UUID 可能落到两个来源。

### 切走再回来也记得住

「刷新保留」靠地址就够了；「切到日志再回来还在」不够——切 tab 必然把 hash 改成 `#logs`，
id 留不到下一次。所以 `localStorage` 里存一条「上次打开」：

```
proxy-rs.lastSession.v1 = { "source": "claude", "id": "<uuid>", "at": <epoch ms> }
```

三条规则，写成代码里的守卫而不是靠调用顺序的巧合：

1. **URL 优先于 storage**——深链接/收藏指向什么就显示什么，storage 只在 URL 没指定会话时兜底。
2. **切 tab 回来不把 id 写回 URL**——`#sessions` 保持干净。这既是直觉，也让 smoke 的
   「切 tab 后 hash 必须精确等于 `#sessions`」这条断言继续成立（任何「进 tab 就回写 id」的
   实现都会把它打红，而且红得很晚）。
3. **失败不写**——地址与 storage 只在详情**读取成功之后**才更新。反过来写的话，一个读不出来
   的会话会被写进地址栏，于是刷新一次失败一次，把瞬时故障变成「这页面坏了」。

`localStorage` 在本仓库是**新约定**（此前前端零 Web Storage）。读写全部 try/catch，不可用时
降级为内存 Map——隐私模式下退化成「刷新丢会话」（与 3.5.5 相同），不是新故障。多标签页不做
`storage` 事件同步，这是已知限制，写在 `ui/README.md` 里而不是假装一致。

---

## 3. 来源可扩展：从一个字面量变成一张表

3.5.5 里「来源」是文案级概念：`session_sources()` 是返回字面量 `json!` 的私有函数，
`dsh_local.available:false` 是手写常量，没有任何代码探测 DSH 目录；`#sessions-sources` 的
按钮甚至**没有 click 监听**。而 `claude` 在 14 处被硬编码（路径解析、id 校验、`GATEWAY_PREFIX`
两份、source 描述符、REST 路径、RPC 名、中文错误子串判 404…）。

新增 `src/sessions/`：

```rust
pub trait SessionSource: Send + Sync {
    fn slug(&self) -> &'static str;            // "claude" —— 进地址
    fn kind(&self) -> &'static str;            // "claude_local" —— 既有 wire，冻结
    fn label(&self) -> &'static str;
    fn gateway_prefix(&self) -> &'static str;  // "claude:" —— stats.db 命名空间
    fn is_valid_id(&self, id: &str) -> bool;   // 拼路径之前必须调用
    fn root(&self) -> Option<PathBuf>;
    fn scan(&self) -> Result<Vec<Candidate>>;  // 同步，一次 depth-1
    fn summary(&self, c: &Candidate) -> Result<SessionSummary>;
    fn read(&self, id: &str, args: &DetailArgs) -> Result<SessionDetail>;
    fn read_subagent(...) -> Result<SessionDetail>;  // 默认拒绝
}
```

关键决定：**`claude_sessions` 一行没搬**。它 1,930 行、17 个测试压着真实转写形状
（`ai-title` / `cost-state` / sidechain / 头尾窗口），搬动只是大半径改名。`sessions::claude`
是一个薄适配层，而解析器留在它自己该在的地方——这正是接缝的意义：新来源**带自己的解析器**，
而不是往共享解析器里加分支（DSH 的 `{type,seq,time,data}` 事件流与 Codex 的 `session_meta`
和 Claude 的行类型一个都不重叠）。

### 可用性只有一处真相

trait 里**没有** `available()`。注册进 `registry()` 就意味着能读；不能读的来源在
`PLANNED` 表里：

```rust
pub(crate) const PLANNED: &[PlannedSource] = &[PlannedSource {
    slug: "dsh", kind: "dsh_local", label: "DSH",
    note: "DSH 会话是 zstd 压缩的本地存档，暂不支持浏览",
}];
```

`sources` 数组由两者拼成，顺序仍是 `[claude_local, dsh_local]`——所以「DSH 是灰色带原因」
这条产品行为**一个字没变**，`tests/admin_api.rs` 与 smoke 的断言原样通过。将来真接 DSH 的
动作是「把一行从 `PLANNED` 挪进 `registry()`」，不是「在 `json!` 里改字面量」。

### 「扩展性」是被测试证明的，不是被声称的

用 `#[cfg(test)]` 的 `FakeSource`（两个会话、一个项目、无子代理）跑通
`registry → scan → group → wire`，断言行上 `source == "fake"`、`groups[].count == 2`、
组 key 为 `fake:proj`。另有一组注册表契约测试：slug 互不相同且 URL 安全、slug 集合与
`PLANNED` 无交集、`gateway_prefix` 不重复、`sources` 顺序稳定。

---

## 4. 一处被结构守卫抓出来的真实依赖环

`tests/admin_api.rs` 有一个模块图守卫：整个 crate 只允许存在**一个**已知环
（`config/credits/session_pool/workbuddy_auth`）。本轮第一次跑集成测试时它红了：

```
left:  [["claude_sessions", "sessions"], ["config", "credits", "session_pool", "workbuddy_auth"]]
right: [["config", "credits", "session_pool", "workbuddy_auth"]]
```

原因是我一开始让 `claude_sessions::read` 持有「列出会话」的实现（它需要 `sessions::plan_groups`
做分组），而 `sessions` 又要用 `claude_sessions` 的解析。这就是一个双向依赖。

修法不是把这个环加进白名单，而是**把方向掰直**：`sessions` 单向依赖 `claude_sessions`，
列表/分组的唯一实现在 `sessions::list_page`；`claude_sessions` 保留细节读取（`read_session`、
`read_subagent`）与格式解析，不再知道「分组」这回事。测试改用
`ClaudeSourceAt`（`#[cfg(test)]`）指向 fixture 目录，于是那些压真实转写形状的测试仍然测着
**控制台真正跑的代码**。

值得记下来的是：这条守卫的价值就在这里——它拦下的不是风格问题，而是一个会让「改一处要记得
改两处」重新长回来的结构决定。

---

## 5. 接口变化（只增不改）

### REST

新增 `/admin/api/sessions`、`/admin/api/sessions/:source/:id`、
`/admin/api/sessions/:source/:id/subagents/:agent`。
旧的 `/admin/api/claude-sessions*` **原样保留**（同一个 handler，默认来源）——
`docs/admin-api.md` 把它们承诺给脚本，只能加不能改。

### 列表参数与响应

新增四个可选参数：`group_by`（`project`）、`group`（组 key）、`group_limit`（缺省 10）、
`source`（slug）。**不带它们时响应与 3.5.5 逐字节一致**，`groups` 与 `group_by` 都是 `null`。

每行新增 `source` 字段（`"claude"`）。分组时 `groups[]` 的字段与语义见
[`docs/admin-api.md`](admin-api.md)。

一个刻意的取舍：**分组模式下 `sessions` 是空数组，不把行拍平再抄一遍**。抄一遍会让 payload
翻倍，而分组模式只会被「发过 `group_by` 的客户端」看到——旧客户端永远不发这个参数。

### 控制台命令

`sessions_list` / `sessions_detail` / `sessions_subagent`。旧名
`claude_sessions_*` **保留为别名**，3.6.0 移除。别名不是懒：3.5.5 发布过的前端 bundle 可能
还活在一个浏览器标签里，而它调的是旧名。

### 打开会话

`open_session_in_browser` 新增可选 `source`（**slug，不是 URL**）。URL 仍由后端拼，两种形态
都支持，`is_console_url` 一个字没改——它只看 `/admin/` 前缀，从不看 hash，所以新增一段既
不放松它也不受它约束。

---

## 6. 不做什么（明确写下来，防止范围蔓延）

1. **不接 DSH / Codex**。真接 DSH 要新增 `zstd`（`Cargo.toml` / `Cargo.lock` 实测均无）=
   往纯 Rust 构建链插 C 库；且本机 396 个 zstd 文件是 **520 MiB 压缩 / 1820 MiB 解压**，
   全量解压 2.99 s、只取每文件首行也要 936 ms——而 Claude 全量头尾读只要 11 ms。这与
   「一个请求只包一次 `spawn_blocking`」的前提直接冲突。Codex 还要额外解决「目录按日期分层、
   分组键必须解压首行、双 uuid 陷阱」。留给下一版。
2. **不做跨来源同项目合并**。契约里留了 `cwd`，但合并规则没有真实数据可验证。
3. **不做组头的 turns/tokens 汇总**（会把列表成本变成 `O(total)`）。
4. **不做折叠状态持久化、多标签页 storage 同步、来源以外的会话过滤。**

---

## 7. 验证

全部实测，不是推断。

| 检查 | 结果 |
|---|---|
| `cargo test` | 16 个套件全过（lib 682 项；`admin_api` 53 → **58**；`session_open_url` 16 → **19**） |
| `cargo clippy --all-targets -- -D warnings` | 干净 |
| `cargo fmt --check` | 干净 |
| `cargo test --test release_consistency` | 4 项通过（8 处版本声明 = 3.5.6） |
| 模块图守卫 | 单一已知环，无新增 |
| 平铺列表兼容性 | 新增测试断言「不带新参数时无 `groups`、行有 `source`、字段集不变」 |
| 分组正确性 | 新增测试断言 `Σ count == total`、组内排序、`more`、组过滤后 `total` 变为组内数 |
| 错误语义 | 未知 `source` / `dsh` / 未知 `group_by` 均 404 且消息含「无效的」（否则会变 500） |
| 旧地址 | `session_open_url` 16 项原样通过，新增 3 项覆盖三段形态与 slug 拒绝 |
| 登录白名单 | 三段链接穿过登录；`nope` / `dsh` / 大写 / 四段 / `?x=1` 全部回落 `/admin/` |
