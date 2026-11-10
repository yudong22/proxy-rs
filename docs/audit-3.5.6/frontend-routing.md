# 3.5.6 前端审计 · 会话路由持久化与二级列表改造点

> 基线 `HEAD=7c94040`（3.5.5）。**只读审计**：除本文件外未改任何文件，未跑 `build:ui`、未 commit。
> 「实测」= 本机无头 Chrome（CDP）跑过，方法见 §6；「读码」= 仅静态阅读。基线全绿：
> `check-ui --runtime` → `no problems found`（55 模块 / 21 样式 / 184 import / 157 id）、
> `check-dist` → `build: 6 files … ok`、`npm run smoke` → `all assertions passed`。只写事实与约束，不写方案。

## 0. 结论先行

1. **点击一行不写 hash。** 全 `ui/src` 只有两处 hash 写入，都在 `ui/src/main.js` 且只写 tab 段
   （`main.js:102`、`main.js:230`）；选中只改模块闭包 `state.selected`
   （`ui/src/views/sessions/index.js:420-422`）。刷新能否恢复唯一取决于地址里是否**已经**带 id：深链接能，
   点击进来必丢（§1.2）。
2. **hash 与选中会分叉。** 深链接进来后点另一行：选中变了、hash 不变，刷新**跳回旧会话**（§1.2）。
   `ui/README.md:142` 的"URL 是唯一真相源"目前对会话选中不成立。
3. **来源维度是纯展示。** `#sessions-sources` 的按钮没有任何 click 监听（全库仅 3 处提及：
   `ui/src/views/sessions/index.js:73`、`ui/src/views/sessions/list.js:40,46`），实测点击后后端调用增量 **0**（§1.3）。
4. **前端零 Web Storage 先例**：`ui/` 下 `localStorage`/`sessionStorage`/`indexedDB` 匹配 **0** 行（§4）。
   "刷新保留"今天唯一承载者是 hash；引入 storage 是**新建约定**。
5. **链接形状被 7 处独立钉死**（3 处 JS + 3 处 Rust + 1 处发布产物测试），另有已发出的
   `open_session_in_browser` 地址与用户收藏：改形状必须全部同步，且旧形状须继续被接受（§2）。
6. **`is_console_url` 对 hash 段零校验**（`src/desktop_hooks.rs:182-201` 只验
   `http://127.0.0.1:<port>/admin/` 前缀）。安全性全压在 `next_hash` / `SESSION_ID_RE` /
   `is_valid_session_id` 三处。
7. **二级列表最大的障碍是分页不是渲染**：服务端 `skip/take`（`src/claude_sessions/read.rs:11-15`）、`total`
   是全局值、前端只持有一页 50 条（`ui/src/views/sessions/index.js:39`），组内计数在**现有 wire 上算不出来**（§3.3）。
8. **smoke 有 4 条断言会被直接打破**（§5.2，集中在 `checkHashRouting`/`checkSessions`）；新增前端文件要"四处
   同步"（§5.1）。

## 1. 会话选择是怎么"发生"的

### 1.1 点击一行 → 只有内存状态，没有 URL

链路：`ui/src/views/sessions/index.js:322-331`（`#sessions-rows` 委托点击）→ `rowFromEvent`
（`ui/src/views/sessions/list.js:167-171` 读 `data-id`）→ `selectSession(id)`，其全部选中逻辑就是
`state.selected = id; renderList(state.list, { selected: id });`（`ui/src/views/sessions/index.js:420-422`）。
hash 写入只有两处，都不是这里——`grep -rn "location.hash\s*=\|history.pushState\|history.replaceState" ui/src`
只命中 `ui/src/main.js:102`（`window.location.hash = key;`，只写 tab 段）与 `ui/src/main.js:230`
（首帧补 `#${DEFAULT_TAB}`）。`switchTab` 还刻意**不**在 tab 相同时重写（`main.js:100-105`），注释明写"在 `#sessions/<uuid>` 上 tab 已是
sessions，重写会把深链接丢掉"——设计上禁止 tab 切换抹掉 id，也就意味着**没人负责补写 id**。

### 1.2 刷新恢复只认地址；切 tab 再回来丢 id 不丢内存

`state` 是模块闭包（`ui/src/views/sessions/index.js:53-62`），刷新即消失；`loadedOnce`（同文件 `:386`）同理。
`main.js` 里读到 id 一共两处：`applyTab` 的 `loadSessions().then(consumeDeepLink)`（`main.js:127`）负责"进入
sessions tab"，`hashchange` 的 `else if (key === 'sessions') consumeDeepLink()`（`main.js:144-148`）负责"已在本
tab 内换 id"；两者都落到 `consumeDeepLink`（`ui/src/views/sessions/index.js:475-484`）→ `deepLinkSessionId()`
（同文件 `:453-458`）→ 命中才选中。实测：

| 步骤 | hash | 高亮行 | 详情面板 |
|---|---|---|---|
| `#sessions` 进入，点第一行 | `#sessions` | 有 | 可见 |
| `Page.reload` 之后 | `#sessions` | **无** | **隐藏** |
| `#sessions/<uuid>` 进入 | 带 id | 有 | 可见 |
| 从该状态 reload | 带 id | 有 | 可见 |

结论：**"刷新保留"当前是深链接的副产物，不是点击行为的一部分。** 需求 (B) 的"点击后刷新仍保留"
等于让点击也产生地址。切 tab 时 `switchTab` 只比较首段，`current !== key` 就写裸 key
（`main.js:100-106`），故 `#sessions/<uuid>` → 别的 tab → 回 `#sessions`：**id 被永久丢弃**；但
`loadSessions` 因 `loadedOnce` 早退（同文件 `:386`）、没有任何清空动作，高亮与详情仍在，再刷新才彻底丢。

**反向分叉（现状不一致，实测）**：深链接 `<A>` 进入后点行 B，实测
`clickedOtherRow = {hash:"#sessions/<A>", active:"<B>"}`，而 reload 之后
`afterRefresh = {hash:"#sessions/<A>", active:"<A>", detailTitle:"会话页测试"}` ——
`state.selected=B`、高亮 B、详情 B，但 `location.hash` 仍是 `<A>`，刷新即回到 A。

### 1.3 来源分段与其它既有事实

- `renderSources`（`ui/src/views/sessions/list.js:39-50`）把 `sources` 渲染成 `.btn-toggle[data-kind]`，
  `available:false` 的加 `disabled` 与 `title`（同文件 `:45-47`）；`state.sources` 写入后**再无第二处读取**
  （`ui/src/views/sessions/index.js:399-400`）。`#sessions-sources` 无 click 监听；实测点击 `claude_local`
  段落后 `__CALLS__` 增量 **0**，hash 不变。
- 会话视图从不读 `appState`（`grep -rn appState ui/src/views/sessions/` 为空）。"当前会话/当前来源"若需跨模块
  共享，将是 `appState` 的首个会话类字段（对照 `ui/src/core/state.js:13-45`；`README.md:104` 限定只放跨模块值）。
- 不存在"来源"的传输层概念：RPC 名 `claude_sessions_list`/`claude_sessions_detail` 带 claude 前缀
  （`ui/src/views/sessions/index.js:389,425`、`src/admin_rpc.rs:458,468`）；`session_sources()` 是后端字面量（`src/admin/ops_sessions.rs:9-19`）。

## 2. 深链接的形状与七处校验

### 2.1 现允许形态

| 形态 | 实例 | 判定者 |
|---|---|---|
| 裸 tab 段 | `#overview` `#logs` `#sessions` `#settings` | `ui/src/main.js:81-84`（`TABS` 白名单，未知→`overview`） |
| tab + 会话 id | `#sessions/<uuid>` | `ui/src/views/sessions/index.js:50,453-458` |
| 登录后落点 | 同上两者 | `ui/server/login.js:56,70`、`src/admin_routes/login.rs:76-99` |
| 打开的浏览器地址 | `http://127.0.0.1:<port>/admin/#sessions/<uuid>` | `src/desktop_hooks.rs:162-172` 生成、`:182-201` 复验、`src-tauri/src/main.rs:213` 第三道 |

UUID 规则三处口径一致但**实现三份**：`ui/src/views/sessions/index.js:50`（JS 正则）、`ui/server/login.js:56`（JS 正则
片段）、`src/admin_routes/login.rs:88-96` → `src/claude_sessions/paths.rs:44-64`（Rust：长度 36、`-` 在 8/13/18/23、
其余小写 hex）。大写、35/37 字符、`file://`、`//evil`、`sessions/../../etc`、`?x=1`、`#x` 全被拒，`tests/session_open_url.rs:249-274` 逐条钉死。

### 2.2 解析实现的两个可扩展性事实

- `deepLinkSessionId` 用 `const [tab, id] = hash.split('/')`（`ui/src/views/sessions/index.js:455`）——**只取第 2 段，
  第 3 段起静默忽略**（实测 `sessions/<uuid>/extra` 被接受）。故"加来源段"若插在中间（`sessions/<source>/<uuid>`）
  会被**读成 source** 判非法 → 静默不选中；若追加尾部（`sessions/<uuid>/<source>`）则**今天就已经被吞掉**。
- `main.js:82` 只看首段并 `toLowerCase()`，注释明写 id 段必须原样透传（`main.js:71-79`）。

### 2.3 安全边界（开放重定向）与断链风险

- 真正的边界只有一条：`next_hash` 的输出是**固定字面量 + 已校验 UUID**，只能拼出自站 `/admin/#…`
  （`src/admin_routes/login.rs:76-99`，理由注释 `:82-87`）；拒绝即回落 `/admin/`（同文件 `:58-63`）。
- `is_console_url`（`src/desktop_hooks.rs:182-201`）是**前缀 + 路径**判定，到 `/admin/` 为止，**不校验 hash**：
  新增 hash 段既不让它失败，也不受它约束。所以来源名只要是**枚举**而非自由字符串，就仍无法表达 scheme/host/traversal。

| 断链风险 | 证据 | 说明 |
|---|---|---|
| 已发出的 `open_session_in_browser` 地址 / 用户收藏 / 复制出的链接 | `src/desktop_hooks.rs:169-171` 硬编 `#sessions/{id}`，`tests/session_open_url.rs:136-144,400-406,430-436,525-530` 逐字节断言；`ui/src/views/sessions/list.js:197-199` 产出 `${origin}/admin/#sessions/${id}`，`ui/tools/smoke/scenarios-views.mjs:187-192` 断言其前缀与后缀 | 旧格式必须继续可选；改格式 = 改 5 处断言 + 已发链接失效 |
| 登录往返 | `ui/server/bridge.js:83-84`、`ui/src/core/ipc.js:115,118` 把当前 hash **原样**带到 `/admin/login`；`ui/server/login.js:70` 的 LANDING 不认新形状则 `landingTab()` 返回 `''` → 登录后落 `/admin/`（`login.rs:58-63`），会话静默丢失 | 改形状必须同时改 LANDING 与 `next_hash` |
| 文档承诺 | `ui/README.md:153-158` 把 `#sessions/<uuid>` 写成"可分享、可收藏、刷新后仍停在同一处" | 与 `main.js:96` 注释（切走即丢弃）已不一致 |
| 开发环回 | `vite.config.js:103-106` 注释：dev 自动登录**故意不传 `next`**，靠浏览器不跳转保住 hash | 新路由若依赖服务端回显 `next`，dev 环回先坏 |

## 3. 二级列表要碰哪些位置

### 3.1 JS（函数签名级别，不写实现）

| 位置 | 现状 | 分组要动什么 |
|---|---|---|
| `ui/src/views/sessions/list.js:136-165` `renderList(page,{selected,empty})` + `:39-50` `renderSources(sources)` | 扁平列表：`rows.map(rowMarkup)` → `outer()` → `innerHTML`（`:143-145`），分页文案 `:147-155`、按钮 `:156-164`；来源段渲染 `.btn-toggle[data-kind]` 但**无 handler** | 入参需加"组"信息（`page` 只有 `sessions/total/offset/limit`）；渲染变两层拼接；来源可切换需新增委托监听（当前**零**） |
| `ui/src/views/sessions/list.js:86-118` `rowMarkup(row)` + `:126-128` `outer(rawSnippets)` | `rowMarkup` 返回 `Raw`，结构 `.sess-row-wrap > .sess-row + .sess-row-open`（`:103-117`）；`outer` 只 join `Raw.value` | 组内行可原样复用；**不能**把组头塞进 `.sess-row`（破 60px 与 `data-id` 语义）；组头须包 `Raw`，否则被转义成 `[object Object]`（`:120-125`） |
| `ui/src/views/sessions/index.js:67-171` `renderSessions()` | 工具栏 `:72-79`、`.sess-split` 双栏 `:81-90`、`.sess-pager` 在 `.sess-rows` **之外** | 组头进 `.sess-rows`（随行滚动）或做成吸顶是两种结构 |
| `ui/src/views/sessions/index.js:39,340-347` | `LIST_PAGE=50`；上/下一页直接改 `state.offset` 后 `force` 重载 | 组内分页需新的 offset 维度（或改服务端契约），单 `offset` 无法表达 |
| `ui/src/views/sessions/index.js:420-441` / `:475-484` | 选中不写 hash；`consumeDeepLink` 只按 `.sess-row[data-id]` 找行（`:482`） | 行被折进跨页/折叠组合后 `scrollIntoView` 找不到行是**静默 no-op**（注释 `:465-473` 已承认是刻意） |

### 3.2 CSS 约束（`ui/style/view-sessions.css`，数值全部实测）

- `.sess-split { grid-template-columns: 260px 1fr }`（`:10-17`）：实测侧栏 **260px**，计算值 `260px 462px`——
  行内新增缩进/组头图标只有 260px 可用。`.sess-rows { flex:1; overflow-y:auto; min-height:0 }`（`:26-30`）是唯一
  滚动容器、**无 padding**。`.sess-row { height:60px; box-sizing:border-box; overflow:hidden; padding:0 12px }`
  （`:35-54`）的 60px 契约写在注释里（`:32-34`）；实测行高 **60**，`.sess-row-wrap` 也是 **60**。
- `.sess-row-wrap { position:relative }`（`:71-73`）只做定位上下文、**不加边框**（`:151-152` 明写）。
  `.sess-row-wrap .sess-row-title { padding-right:26px }`（`:119-121`，实测 `26px`）为右上角 22px 快捷按钮让位，
  注释要求内缩**只在 title 上**、不得加到 `.sess-row` 以免破 60px 盒 → 组头复用 `.sess-row-title` 会带走 26px。
- `.sess-row-path`（`:163-172`）用 `direction: rtl` 做左端省略；`.sess-row-foot`（`:175-190`）单行裁剪。
  `.sess-pager`（`:205-210` 为 `display:flex; padding:8px 10px; border-top`）在 `.sess-rows` 之外
  （`ui/src/views/sessions/index.js:86-89`），故底部固定、不随列表滚动。
- 行外子元素**只有一个样式**：`.sess-empty { padding:24px 14px; text-align:center }`（`:242-247`）——组头没有
  既有选择器可复用；`.sess-sources` 自身**无任何 CSS 规则**（grep `sess-sources` 在 `ui/style/` 下为空），靠外层
  `.view-mode-group`（`ui/style/logs.css:98-108`，高度锁 29px 对齐 `.btn-small`）。

### 3.3 "组"的键与分页冲突

行里可用的组键只有两个且不总是同时在场：`cwd` 与 `encoded_project`，`ui/src/views/sessions/list.js:94` 取
`cwd || encoded_project`（`src/claude_sessions/types.rs:39-44` 说明 `cwd` 可为空、且**不允许**用目录名反推）。
本机实况：`~/.claude/projects` **13 个目录 / 31 个 jsonl**，最大目录 10 个会话（§6）。`total` 是全局计数、
`sessions` 只是一页（`src/claude_sessions/read.rs:11-15` 全量排序后 `skip/take`；`src/admin/ops_sessions.rs:86-97`
原样转发）。前端因此**无法**从一次响应推出任一组的会话数或组总量：一页 50 条可能混着若干组的碎片。要"组头显示
N 个会话 + 组最近活动"，只能由服务端在 wire 上补字段，或前端额外拉全量（会与 `LIST_PAGE` 分页语义打架）。
后端成本与 wire 契约见 `docs/audit-3.5.6/backend-sources.md` §3–§4。

## 4. "刷新保留"放哪：hash 还是 storage

### 4.1 先决事实：没有先例

`grep -rn "localStorage\|sessionStorage\|indexedDB" ui/` 无输出（exit=1，含 `ui/src`、`ui/server`、`ui/tools`、
`ui/*.html`）；`grep -rn "document.cookie" ui/ src/` 同样 0 行（cookie 只由服务端 Set-Cookie 下发，`login.rs:221-224`）。
`ui/src` 里全部"记忆"手段只有三类：**hash**、**模块闭包**（`ui/src/core/state.js:13-45`、
`ui/src/views/sessions/index.js:53-62`）、**服务端 cookie**。另一个运行期全局 `window.__PROXY_RS_SERVER__`
（`ui/src/core/ipc.js` 注释、`ui/src/views/sessions/list.js:29-31`）由服务端桥接注入，不是持久层。

### 4.2 选项对比（**选项，不是决定**）

| | A. 只扩 hash | B. hash + storage 分工 | C. 只用 storage |
|---|---|---|---|
| 承载"哪个会话" | hash | hash | storage |
| 承载列表 UI 态（来源筛选/分页 offset/组折叠） | 需全塞进 hash | storage | storage |
| 可分享/可收藏 / 刷新保留 | 是（`README.md:153-158` 已承诺）/ 是 | 是 / 是 | **否** / 是 |
| 登录往返保留 | 是，但**必须**同步 `ui/server/login.js:70` + `login.rs:76-99`，否则静默丢链 | 同 A（storage 自动存活，但 URL 不带 → 复制给别人无效） | 是（跨设备不迁移；共享浏览器会串） |
| 新增历史条目 | 会：实测 `location.hash` 赋值使 `history.length` **2→3** | 可用 `replaceState` 规避（`main.js:230` 已有先例） | 不会 |
| 破链风险 | 高（§2.3 全表） | 中（旧格式须兼容 + 新增键） | 低（URL 不动），但复制链接行为与今天不一致 |
| 需新建约定 / 与现有约束 | 解析器（第 3 段规则）/ 一致（`README.md:142` 已把 URL 定为唯一真相源） | 解析器 + 存储模块（键名/版本/清理/多标签页同步）/ 需改口径：URL 只做"可分享的那一半" | 存储模块 / 需改口径，且与 `main.js:96`、`README.md:142` 冲突 |

若选 B/C，需要明确的约束（仍为**待定项**，非建议）：键名不得与既有 `#id`/类名冲突，且 `check-ui` 不看 JS
字面量 → 须在 `ui/README.md` 新增登记节；版本化（如 `…v1`）与"来源已下线 / 会话文件已删"的清理路径必须定义；
`sessionStorage` 只在单标签页存活、不满足多标签页一致性，`localStorage` 则须处理 `storage` 事件或接受各标签页
各自为政。另：`selectSession` 已有可观测失败路径（`ui/src/views/sessions/index.js:436-440`：
`renderDetailMessage` + error toast），"从 storage 恢复了已删除的 id"会走到这里。

## 5. 测试与守卫

### 5.1 `ui/tools/check-ui.mjs` 对新增文件的具体要求

| 规则 | 行 | 对新增文件意味着 |
|---|---|---|
| 每个 `.css` 必须被 `ui/index.html` 链接 | `:103-110` | 新样式文件漏 `<link>` = 直接失败 |
| 每个 `.css` 必须登记进 `CASCADE`，顺序须是子序列且 `tokens.css` 第一 | `:125-129`（数组 `:53-83`）、`:112-122` | 漏登记或顺序错都失败，顺序有语义 |
| 静态 `id="literal"` 跨**不同文件**重复即失败；同文件内允许 | `:131-153` | 组头若用固定 id 只能在一个模块里定义；动态 id 不检查（`:141`） |
| 每个相对 import 必须存在；命名 import 必须真被导出 | `:155-195` | 只识别 `export function/class/const/let/var X` 与 `export {…}`（`:158-166`）——**`export * from` 不被识别**，经它转出的名字会被误报缺失 |
| `--runtime`：静态 `$('#id')` 必须能在渲染后页面找到，或该 id 在任一模块/`index.html` 里静态定义 | `:197-331`（判定 `:314-319`） | 只存在于变量里的 id 会失败；"按需创建"元素只要被静态定义即可 |

`ui/tools/check-dist.mjs` 另有两条会拦新样式：只允许一个打包 CSS（`:70-71`），每个源样式文件都要有 marker
（`:79-113`，"linked but no marker" = 失败 `:108-111`）；产物只能落 `src/ style/ assets/`（`:139-146`）。即
"新增一个 CSS 文件" = 改 4 处：`ui/index.html`、`check-ui.mjs` 的 `CASCADE`、`check-dist.mjs` 的 `markers`、`README.md:47-71` 的表格。

### 5.2 会被二级列表/路由改动直接打破的断言（逐条）

`ui/tools/smoke/scenarios-views.mjs` — `checkSessions`：

| 行 | 断言文案 | 钉死的期望值 | 被什么打破 |
|---|---|---|---|
| `:115`/`:128` | `sessions list renders one row` / `every session row carries an open-in-browser button` | `#sessions-rows .sess-row` = `1`；`#sessions-rows .sess-row-wrap .sess-row-open` = `1` | 组头也带 `.sess-row` 类、默认折叠导致行重建、或组内行不再用 `.sess-row-wrap` |
| `:118` | `DSH source is disabled` | `#sessions-sources [data-kind="dsh_local"]`.disabled = `true` | 来源改为可切换（去掉 `disabled`）立即失败 |
| `:129`/`:133`/`:184`/`:196` | `the open button targets that row id` / `the detail header offers the same action` / `server console: …` | `data-open-id` / `data-copy-id` = `b9c3800a-311e-4c89-9a38-c533d8969b5d` | `data-*` 若改成复合键（含来源） |
| `:187-192` | `server console: the copied link is this console’s deep link` | `copied.endsWith('/admin/#sessions/<uuid>')` 且 `startsWith('http://127.0.0.1:')` | `ui/src/views/sessions/list.js:197-199` 链接形状一改即失败 |

`tool output never scrolls (collapse instead)`（`:122`，期望 `#session-timeline .sess-pre` 中
`scrollHeight > clientHeight+2` 个数 = `0`）只在组头引入滚动容器并影响时间线布局时才打到。
`checkHashRouting`（同文件 `:215-259`）——本轮最硬的四条：

| 行 | 断言文案 | 期望值 | 说明 |
|---|---|---|---|
| `:255` | `a #sessions/<uuid> link opens the sessions tab` | active tab = `'sessions'` | 来源段若使首段判定失败会连锁 |
| `:256` | `a deep link keeps its session id in the hash` | `location.hash.includes(<uuid>)` = `true` | 断言宽松（子串），加来源段仍可过；**hash 被规范化成无 id 形态**必失败 |
| `:257` | `a deep link selects that session` | `.sess-row.active[data-id]` = `<uuid>` | 依赖"深链接→选中"链，任何解析改动先打这里 |
| `:258` | `a malformed session id still resolves the tab` | `'sessions'`（输入 `sessions/not-a-uuid`） | 非法 id 若改为落到错误页/空来源页则失败 |

`ui/tools/smoke/scenarios-pages.mjs:89` — `pane ${tab}: routed to #${tab}`，期望 `location.hash === '#sessions'`。
**最容易被无意打破的一条**：只要"进入 tab 即自动选中并回写 `#sessions/<id>`"，此断言立刻红。

`ui/tools/smoke/scenarios-states.mjs`（`checkEmptySessionList`）：`:124` `emptyText.includes('没有找到')`（对应文案
`ui/src/views/sessions/index.js:409`，换成别的词即失败）；`:125` `#sessions-rows .sess-row` = `0`——空列表下若渲染
任何带 `.sess-row` 类的组头即失败。

`ui/tools/smoke/scenarios-integration.mjs:52,62` — `paletteOnSettings: window.location.hash === '#settings'`
（命令面板跳转后 hash 必须**精确**为 `#settings`）。`ui/tools/admin-smoke.mjs`（CI 第二步，真网关）：`:357-365`
逐 tab 点击并断言 `#tab-<key>` 文本长度 `> 50`（宽松，不易破）；`:371` 要求
`claude_sessions_list{args:{limit:5}}` 必须路由成功——RPC 名若带来源维度会失败。

迁移成本：改动集中在 **2 个函数 + 1 条断言簇**——`renderList` 与 `selectSession/consumeDeepLink` 是唯一的渲染/选中
入口，真正需重写期望值的是 `checkHashRouting:257-258` 与 `checkSessions:115,128`；`scenarios-pages.mjs:89` 属于
"必须保持不变"的约束，不是"可以改的期望值"。

### 5.3 `ui/README.md` 第五节要同步的内容

表格与说明在 `README.md:140-158`：表格（`:144-149`）只有四个裸 tab 地址，**没有** `#sessions/<uuid>` 行——深链接
只在正文 `:153-154` 描述。`:142`"URL 是路由的唯一真相源"与 `:154`"刷新后仍停在同一处"：后者只有深链接成立、
前者对点击选中不成立（§1.2）。若 3.5.6 要"点击也写地址"这两句才第一次成为事实；若改用 storage 必须改口径。
`:155-158` 记录登录页 `next` 白名单并明确"多接受一种形态并不放宽开放重定向约束"——新增来源段后这句仍是论证
义务（`login.rs:82-87` 是同一论证的另一半）。

## 6. 复现命令

```bash
cd /Users/denis/Sites/proxy-rs
grep -rn "location.hash\s*=\|history.pushState\|history.replaceState" ui/src   # 仅 2 处，均在 main.js
grep -rn "localStorage\|sessionStorage\|indexedDB" ui/                          # 0 行
grep -rn "sessions-sources\|data-kind" ui/src/                                  # 仅 3 处，均非监听
grep -rn "appState" ui/src/views/sessions/                                       # 0 行
grep -n "admin/#sessions" tests/session_open_url.rs ui/tools/smoke/scenarios-views.mjs
node ui/tools/check-ui.mjs --runtime && node ui/tools/check-dist.mjs && npm run smoke   # 应全绿
ls ~/.claude/projects | wc -l; find ~/.claude/projects -maxdepth 2 -name '*.jsonl' | wc -l  # 13 / 31
```

运行期数字（§1.2–§1.3、§3.2）来自一次性 CDP 探针：`http.createServer` 托管 `ui/` → 无头 Chrome
`--headless=new --remote-debugging-port` → `Page.addScriptToEvaluateOnNewDocument` 注入 `window.__TAURI__` 与
`ui/tools/smoke/fixtures.mjs` 的 `MOCK`（与 `ui/tools/smoke.mjs:227-290` 注入完全相同）→ `Page.navigate` 到
`#sessions` / `#sessions/<uuid>` → `Page.reload` → `Runtime.evaluate` 读 `location.hash`、`.sess-row.active`、
`#session-pane[hidden]`、`getBoundingClientRect().height`、`getComputedStyle(...).paddingRight`、`history.length`。
探针为临时文件、**未入库**；每条结论都已配静态证据，CDP 只把"读码推断"升级为"实测"。

