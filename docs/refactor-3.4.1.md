# proxy-rs 3.4.1 重构记录：超大文件拆分与可扩展性

> 基线：`main` @ `2a8ba3d`（v3.4.0）
> 目标：把妨碍扩展的超大文件拆成职责内聚的模块，**行为零变更**
> 结果：17 个提交，`cargo test` 755 全绿（原 752，+3 守卫），五项门禁全过

---

## 1. 为什么做这件事（问题不是"行数"）

行数本身不是缺陷。真正的成本是三个具体症状，每个都能对应到下表的数字：

1. **"该放哪"无法回答。** `workbuddy_auth.rs` 3842 行里塞了 8 件互不相关的事
   （类型、偏好、api-key 池、文件存储、token 刷新、扫码登录、积分、打卡）。
   新增一个"账号导入格式"功能时，作者只能靠 grep 猜位置。
2. **冲突面积。** 多个功能并行开发时，都改同一个 3000 行文件的概率远高于改
   不同的小文件。本次重构期间就真实发生了：另一个会话在改
   `ui/src/views/settings/credential-pool.js`（929 行），而它正是准备拆分的对象。
3. **阅读成本。** `src/proxy.rs` 里 `forward_request` 单函数 851 行——
   任何一次改动都要在 851 行的上下文里定位。

判据因此不是行数，而是 **"是否有多件事被塞进一个命名"**。这条判据在 3.3.0
审计里已经确立，3.4.1 沿用它，并明确**不拆**那些"只是长但确实只有一件事"的文件
（见 §5）。

---

## 2. 后端拆分（Rust）

### 2.1 契约：兼容层保证零调用方改动

每个拆分都遵循同一模式，这也是它能做到零行为变更的原因：

```
src/foo.rs  →  src/foo/mod.rs     模块文档 + 布局表 + mod 声明 + pub use 子模块
               src/foo/a.rs        use super::*;
               src/foo/b.rs
```

`mod.rs` 把子模块重新导出，因此**所有历史 `crate::foo::X` 路径继续解析**，
`tests/`、`src-tauri/`、以及库内其它模块的调用一行都不用改。这是本次重构能
**零测试改动**的机制基础。

### 2.2 逐项结果

| 文件 | 原行数 | 拆分后 | 子文件最大 |
|---|---:|---|---:|
| `workbuddy_auth.rs` | 3842 | `workbuddy_auth/{types,prefs,api_keys,store,refresh,oauth,points,checkin}` | 490 |
| `stats.rs` | 3433 | `stats/{record,metrics,connection,requests,sessions,day,writer}` | 352 |
| `translate/pipeline.rs` | 2522 | `translate/pipeline/{policy,routing,sanitize,normalize}` | 386 |
| `translate/responses.rs` | 2314 | `translate/responses/{request,response,stream}` | 490 |
| `proxy.rs` | 2420 | `proxy/{flavor,handlers,request,forward,response,models}` | 858 |
| `admin.rs` | 1036 | `admin/{ops_settings,ops_identities,ops_client_accounts,ops_pool,ops_dsh,ops_points,ops_sessions,ops_stats}` | 376 |
| `admin_routes.rs` | 1024 | `admin_routes/{login,observe,settings,identities,automation,client_accounts,pool,error}`，路由表留在 `mod.rs` | 264 |
| `growth.rs` | 1320 | `growth/{protocol,tasks,events}` | 459 |
| `scheduler.rs` | 976 | `scheduler/{loops,daily_checkin,login_scan,growth_tasks,engine,snapshot}` | 234 |
| `credits.rs` | 828 | `credits/{fields,shapes,parse,fetch,handler}` | 295 |
| `src-tauri/main.rs` | 1081 | `app/{context,tray,server,lifecycle}` + `main.rs` 保留 IPC 命令与 `main()` | 277 |

### 2.3 几个值得记录的判断

**`stats.rs` 不按"读 / 写"拆。** 3.3.0 审计的结论是它**不是** god module：
边界是真实的（同一张 `request_logs` 表）。按读写硬切会造出两个争抢同一连接池的
模块，反而增加耦合。所以拆的维度是**查询主题**，`StatsDb` 仍是**一个类型**，
方法分散在多个 `impl StatsDb` 块里——这是 Rust 里给单一类型分文件的惯用做法。

**`proxy.rs` 的 `forward_request` 没有拆开。** 拆分后 `forward.rs` 仍有 858 行，
其中约 851 行是这一个函数。它内部是紧耦合的控制流（凭据尝试循环、401 特殊分支、
free 节点降级）。切开它必须改控制流，那已经不是"搬文件"而是重写核心路径。
这里如实记为已知债务，而不是假装拆干净了——见 §5。

**`admin/` 子模块用 `ops_` 前缀。** 因为 `mod.rs` 需要 `use crate::settings`
和 `use crate::client_accounts`，子模块若也叫 `settings` / `client_accounts`
会直接撞名（`E0255`，实测撞过）。前缀让"admin 的操作"与"底层存储模块"在路径上
就能区分。

**`src-tauri` 的两个框架约束（不是偏好）：**
1. `#[tauri::command]` 必须在**同一模块**定义并被 `generate_handler!` 注册——
   宏在定义处生成 `__cmd__<name>` 辅助宏，跨文件注册报
   `cannot find macro __cmd__get_bootstrap_info`。所以 5 个命令留在 `main.rs`。
2. `include_bytes!("../icons/…")` 在子模块目录下要多上一级。

---

## 3. 前端拆分

### 3.1 先修基础设施：资产清单自动化

**这是本次最重要的一个改动，因为它决定了前端能不能拆。**

`src/console.rs` 原来手写维护 47 条 `(url, content-type, include_str!)` 三元组。
前端无 bundler，所以：

- 在 Tauri 下新增 `ui/src/x.js` 能正常 import（Tauri 直接读目录）；
- 在服务器控制台会 **404** —— 不是编译错误，也没有任何测试会失败。

也就是说**任何一次前端拆分都会静默产生 404**。所以先做这一步：

`build.rs` 枚举 `ui/{src,style,assets}` 三个服务根 + `index.html` +
`server/bridge.js`，生成 `${OUT_DIR}/ui_assets.rs`，`console.rs` 用
`include!` 引入。`cargo:rerun-if-changed=ui` 让增删文件都触发重建。

配套三个守卫测试：
- `the_asset_manifest_covers_the_ui_tree` —— 清单与 `ui/` 目录**双向**一致
- `every_asset_lives_under_a_routed_prefix` —— 每条都有 console 路由能到
- `the_index_html_carries_the_rewrite_target` —— `console_page` 的改写锚点仍在

未知扩展名直接 **panic 构建**，而不是静默漏掉一个文件。

### 3.2 JS：薄入口 + 子目录

前端同样用"入口只 re-export"保住 export 面，于是**所有外部 import 一行不改**：

| 文件 | 原行数 | 入口 | 子模块 |
|---|---:|---:|---|
| `credential-pool.js` | 950 | 25 | `pool-table 344 / growth 235 / automation 195 / drag 123 / controls 100 / status 17` |
| `pool-transfer.js` | 520 | 33 | `export-dialog 255 / import-dialog 187 / shared 85` |
| `overview.js` | 529 | 23 | `speed 244 / cards 103 / status 77 / stats 35` |
| `client-accounts.js` | 413 | 33 | `list 143 / reveal-dialog 132 / create-dialog 111 / shared 46` |
| `tools/smoke.mjs` | 1090 | 254 | `fixtures 330 / scenarios-integration 326 / scenarios-boot 260 / scenarios-views 129` |

**顺带消除两处本次新引入的循环依赖。** 拆的过程中
`pool-table ⇄ drag`、`list ⇄ reveal-dialog` 各形成了一个 ES module 环。运行上
是安全的（函数声明提升、只在事件处理器里调用），但那是**新增的架构事实**，
不该悄悄留成既成事实。按项目已有的注入惯例
（`setOAuthPoolRefresh` / `setReloginPoolRefresh`）改成注入回调：
`setPoolRepaint` / `setAccountsRepaint`。

### 3.3 CSS：按既有分节注释切

`views.css 1230 / workbuddy.css 653 / logs.css 455` → 14 个文件。

切分依据是**文件里现成的分节注释**，不是主观分类，所以每条规则都留在原本的
相对位置。层叠顺序是语义的一部分，因此 `index.html` 的 `<link>` 与
`check-ui.mjs` 的 `CASCADE` 数组必须同步修改。

---

## 4. 怎么证明"零行为变更"（不靠目测）

每个拆分都做了**规范化 diff**：去掉注释、去掉 `pub`/`pub(crate)` 前缀、
去掉 `use` 行后，逐行对比新旧代码。判定标准是 `only-old = 0`。

| 拆分 | 生产代码 only-old | 测试代码 only-old / only-new |
|---|---:|---|
| workbuddy_auth | 0 | 0 / 0（1137 行测试逐字节未改） |
| stats | 0 | 0 / 0（1712 行） |
| translate/pipeline | 0 | 0 / 0（1601 行） |
| translate/responses | 0 | 0 / 0（1259 行） |
| proxy | 0 | `tests.rs` 2617 行 `git diff` 为空 |
| admin | 0（仅 import 行） | 0 / 0 |
| admin_routes | 0 | 无测试模块 |
| growth | 0 | 0 / 0（10487 字节） |
| scheduler | 0 | 0 / 0（7802 字节） |
| credits | 0 | 0 / 0（2518 字节） |
| ui/src 四个模块 | 0 | — |
| src-tauri/main.rs | 2（`include_bytes!` 路径） | — |

几处唯一非零项的说明：
- `select_model` / `resolve_session` / `refresh_one_key_points`：只是长签名被
  `rustfmt` 折行，行数差异而非内容差异。
- `src-tauri` 的 2 行：子模块目录深了一层，`../icons/` 必须变 `../../icons/`。

**CSS 用了更强的证明**：把旧 10 个文件按层叠顺序拼接、新 20 个文件按
`CASCADE` 顺序拼接，两者**字节完全相同**——89,091 字节、
sha256 `bba36599…a6123f`。浏览器解析到的样式流一个字节没变。

---

## 5. 明确**不**做的事（附理由）

写进文档是为了避免下个版本重新讨论一遍。

| 不做 | 理由 |
|---|---|
| 拆 `forward_request`（851 行） | 内部是紧耦合控制流，切开必须改逻辑。风险与收益不匹配，须配合新写测试单独做 |
| 按读写拆 `stats` | 会造出两个争抢同一连接池的模块，反而增加耦合。它只有一件事 |
| 拆 `src/proxy/tests.rs`（2617 行） | 测试文件的"行数"不是维护成本的主因；按被测函数分文件会让跨函数的行为测试无处安放 |
| 给 `translate` 加抽象层 | 它已经是纯函数 + policy，再加一层是过度设计 |
| 统一 admin 的 REST 与 RPC 两份 handler | 3.4.0 已决定两者共用同一实现，3.4.1 不改变这个决策 |

---

## 6. 剩余的"大文件"与判断

拆分完成后按生产行数排序，头部是：

| 文件 | 生产行数 | 判断 |
|---|---:|---|
| `src/proxy/tests.rs` | 2617 | 测试文件，见 §5 |
| `src/proxy/forward.rs` | 858 | 单函数 `forward_request`，见 §5 已知债务 |
| `src/admin_rpc.rs` | 777 | 命令分发表，本身就是薄 dispatch。**不该拆** |
| `src/growth_runner.rs` | 668 | 编排层：预算、单账号推进、池级运行、汇总。可再按「单账号 / 池级」切 |
| `src/pool_transfer/import.rs` | 647 | 已在 3.4.1 内拆到位；导入链本身是一条长流程，再切会把一次事务拆散 |
| `src/console.rs` | 626 | 控制台路由 + 静态资源服务。真正的资产清单已移入 `build.rs` |
| `src/codex_config.rs` | 606 | 单一职责（写 Codex 配置），长但只有一件事 |
| `src/proxy/sse.rs` | 599 | SSE 分帧 + 请求台账 + TTFT 追踪，内聚 |

前三个是**明确的下一步候选**；后四个属于「长但只有一件事」，行数不是病灶。
判据始终是"是否多件事共用一个命名"，而不是把所有文件压到某个行数以下。

值得注意：`admin_routes.rs`（1023）、`growth.rs`（994）、`scheduler.rs`（782）、
`credits.rs`（753）在初版文档里被列为"下一步候选"，已在同一轮内完成拆分。

---

## 7. 验收

| 检查 | 结果 |
|---|---|
| `cargo test --all-targets` | **755 passed**（基线 752 + 3 个资产守卫） |
| `cargo fmt -- --check` | 通过 |
| `cargo clippy --all-targets -- -D warnings` | 零告警 |
| `node ui/tools/check-ui.mjs` | `no problems found`（55 JS 模块 / 20 样式表 / 183 imports） |
| `node ui/tools/smoke.mjs` | `all assertions passed`（85 断言，顺序未变） |
| `cargo check -p proxy-rs-gui` | 通过（桌面端可编译） |
| 依赖环 | 未新增（`the_module_graph_has_no_new_cycles` 通过） |

---

## 8. 给后续开发者的操作要点

1. **新增前端文件不需要改 Rust。** 放进 `ui/src` / `ui/style` / `ui/assets`
   即被 `build.rs` 收录。
2. **新增样式表要改两处**：`ui/index.html` 的 `<link>` 与
   `check-ui.mjs` 的 `CASCADE`，两者顺序必须一致。
3. **新增 Rust 子模块**照现有模式：`mod x;` + `pub use x::*;`，子文件开头
   `use super::*;`。跨文件私有的项改成 `pub(crate)`。
4. **拆分后必须跑规范化 diff**，不要只信测试通过——测试覆盖不到的地方，
   只有逐行比对能发现。
