# 3.5.6 后端审计 · 会话来源抽象与二级分组聚合

> 基线 `HEAD=7c94040`（3.5.5），`git status --porcelain` 空。**只读审计**：除本文件外未改任何文件。
> 「实测」= 本机跑过，命令在 §7；「推断」= 读代码得出、未跑。不写方案，只写事实与约束。

## 0. 结论先行

1. **二级分组只需 `stat`。** `candidates()` 一次 depth-1 walk 已同时给出 `encoded_project` 与 `mtime`（`paths.rs:111-152`），
   补上 `meta.len()` 即得「组内会话数 / 组最近活动 / 组字节总量」。**额外成本实测 ≈1.0 ms/31 会话、1.5 ms/300 会话**。
2. **但分组键不能信目录名。** 编码有损（`/`→`-`），`/a/b/c` 与 `/a-b-c` 同目录名，本机已存在此模糊目录；更硬的是
   **同文件内 `cwd` 会变**：实测 27 个可全读 transcript 里 **2 个**中途换 `cwd`。
3. **分页 × 分组是结构冲突。** 现在是「全量排序 → `skip/take` → 只读命中行」（`read.rs:11-16`），组级计数却要对全量算。
   且 `tests/admin_api.rs` **无一条断言**检查 `total`/`offset`/`limit`（grep 空），该兼容承诺只由 `list.js:147-163` 承担。
4. **「来源」目前是文案级概念。** `session_sources()` 是返回字面量 `json!` 的私有函数（`ops_sessions.rs:9-19`），与读路径零耦合：
   `dsh_local` 的 `available:false` 是手写常量，无任何代码探测 DSH 目录。`claude` 硬编码共 14 处（§2）。
5. **3.5.6 真读 DSH 不划算。** 实测 396 个 zstd / **520 MiB 压缩** / **1820 MiB 解压**，全量解压 **2.99 s**，仅取每文件首行也 **936 ms**；Claude 全量 head+tail 读只要 **11 ms**。且 DSH 一半 id 过不了 `is_valid_session_id`。
6. **`ListPage` 不参与序列化**：wire 是 `ops_sessions.rs:86-97` 手拼的 `json!`。给 `ListPage` 加字段**不会**出现在网络上
   ——本次最易踩的静默失败（§1）。

## 1. 现有读取链路

**两条入口，一个实现**：`index.js:389` `invoke('claude_sessions_list')` →（网页）`POST /admin/api/rpc/<cmd>`（`console.rs:114`）→ `admin_rpc.rs:458` `from_value::<ListArgs>` → `admin::claude_sessions_list`；脚本入口 `GET /admin/api/claude-sessions?limit=&offset=`（`admin_routes/mod.rs:110`）→ `observe.rs:77-89` → 同一实现（`ops_sessions.rs:73-98` 列表／`:101-119` 详情／`:122-136` 子代理）。三个 RPC arm：`admin_rpc.rs:458,468,479`；三条 REST：`mod.rs:110-115`。`admin_rpc.rs:6-13` 说明命令名**故意**等于 Tauri 签名
（改名要动 UI 47 处）。

**模块**（表见 `claude_sessions.rs:54-61`）：`types.rs`(234) wire 结构与常量｜`paths.rs`(154) source/id 校验/id→路径/walk｜
`parse.rs`(268) `parse_iso_ms`/取值/截断/`read_lines`｜`summarize.rs`(495) `Accum` 单遍折叠+`summary_from_file`｜
`read.rs`(207) 三个入口+gateway join｜`claude_sessions.rs`(572) 文档+重导出(1-89)+tests(91-572)。`claude_sessions.rs:85-89` 五个 `pub use`、各子模块 `use super::*`（`:62-66`）→ **彼此可自由互调，无模块边界**。

**wire 结构（全在 `types.rs`）**

| 结构 | 位置 | 字段 / 要点 |
|---|---|---|
| `SessionSummary` | `:37-63` | 16 字段：`id, encoded_project, cwd, title, first_seen_ms/last_seen_ms/file_mtime_ms, size_bytes, git_branch, version, turns, assistant_turns, tool_calls, subagents, last_model, counts_partial`（Option 者：first/last_seen_ms、git_branch、version、last_model） |
| `SessionRow` | `:184-192` | `#[serde(flatten)] summary` + `gateway:Option<SessionUsageTotal>` + `gateway_error:Option<String>`；flatten 令会话字段平铺在行顶层，UI 两种形状都吃（`list.js:87-89`：`row.summary \|\| row`） |
| `ListPage` | `:195-205` | `available, reason, root, stats_available, total, offset, limit, sessions`。⚠ **从不直接序列化**——`ops_sessions.rs:86-97` 手写 `json!` 重新展开，并混入**不在结构体里**的 `sources`、`gateway_error` |
| `SessionDetail` | `:208-227` | summary/cost_state/models/tokens/tokens_incl_subagents/timeline/timeline_offset/timeline_total/has_more/ignored/subagents/agent_id/gateway/gateway_error；走 `to_value`（`ops_sessions.rs:118`），无额外字段 |
| `ListArgs`/`DetailArgs` | `:168-177` | 各两个 `Option<usize>`，共享 `ClaudeSessionsQuery`（`observe.rs:120-124`） |
| `Candidate` | `:229-234`（`pub(crate)`） | `path, encoded_project, mtime, id` —— **无 size_bytes** |

**一次请求 8 步**：①`from_env()`（`paths.rs:26-37`：`$CLAUDE_PROJECTS_DIR`→`~/.claude/projects`，`is_dir()` 假则 `None`→
`sessions_unavailable`，`ops_sessions.rs:77-81`）②`spawn_blocking` **整个请求一次**（`ops_sessions.rs:42`；`claude_sessions.rs:17-23`禁止每文件一次）③`candidates()`（`paths.rs:111-152`：depth-1、不跟 symlink、跳 `.` 目录，只收 `.jsonl` 且 stem 过 UUID 校验，
每文件一次 `metadata()`）④排序 `(mtime desc, id desc)`（`read.rs:12`）⑤`skip(offset).take(limit)`，**只对命中行**读（`read.rs:16`）⑥`summary_from_file`（`summarize.rs:428-479`：≤512 KiB 全读，否则头尾各 256 KiB 各解析一次，两段都没 title 再全量重读
`:458-463`；每次还调 `count_subagents` `:481-495`）⑦`gateway_ids`（`read.rs:40-44`）+ 第二次 `spawn_blocking` 打 `stats.db`（`ops_sessions.rs:64`；SQL `stats/sessions.rs:199-208`）⑧手拼 `json!`。**无任何缓存**：grep `OnceCell`/`Mutex<HashMap`/`cache` 只命中 token 字段名（`parse.rs:156-157`、`types.rs:69-70`）；`index.js:1-12` 声明不轮询、靠刷新按钮。

**`source` 描述符唯一来源**：`ops_sessions.rs:9-19` 字面量 `json!`，两 kind 写死（`claude_local` true/null；`dsh_local` false/
"DSH 会话是 zstd 压缩的本地存档，暂不支持浏览"），用于 `:26` 与 `:90`。前端仅两处消费：`list.js:30-50`（渲 `data-kind` 段按钮）、
`scenarios-views.mjs:69`（断言 `disabled===true`）。

**常量**：`GATEWAY_PREFIX="claude:"`(`types.rs:6`)｜`DEFAULT_LIMIT=50`(`:8`)｜`MAX_LIMIT=200`(`:10`)｜`HEAD_TAIL_BYTES=256KiB`(`:16`)｜`MAX_LINE_BYTES=32MiB`(`:20`)｜`TEXT_CAP=4000`(`:23`)｜`DETAIL_LIMIT=200/1000`(`:27-29`)｜`limit` 被 `clamp(1,MAX_LIMIT)`(`read.rs:9`)；前端写死 `LIST_PAGE=50`（`index.js:41`）。

## 2. 「来源」可插拔：`claude` 硬编码在哪（14 处）

| # | 位置 | 硬编码 | 抽象面 |
|---|---|---|---|
| 1 | `paths.rs:26-37` | `$CLAUDE_PROJECTS_DIR` → `~/.claude/projects` | 要 |
| 2 | `paths.rs:10-12` | `struct ClaudeSessionsSource{ projects_root }` 单字段 | 要 |
| 3 | `paths.rs:44-64` | `is_valid_session_id`：只认严格小写 hex 8-4-4-4-12 | **要（最关键）** |
| 4 | `paths.rs:82-107` | `<root>/<任意项目>/<id>.jsonl`，逐目录 `canonicalize` | 要 |
| 5 | `paths.rs:111-152` | `candidates()` 只收 `*.jsonl` + UUID stem | 要（枚举唯一入口） |
| 6 | `types.rs:6` | `GATEWAY_PREFIX="claude:"` | 要（`read.rs:42,54`、`ops_sessions.rs:114` 三处消费） |
| 7 | `summarize.rs:62-96` | 行类型分派 `ai-title`/`cost-state`/`user`/`assistant`/`system` + `:98-115` 读 cwd/gitBranch/version | 要（解析差异本体） |
| 8 | `ops_sessions.rs:9-19` | `session_sources()` 两 kind 字面量 | 要 |
| 9 | `ops_sessions.rs:79,107,128` | 错误文案含 `~/.claude/projects` | 要（受 #14 约束） |
| 10 | `ops_sessions.rs:114` | 第二份 `format!("claude:{}")`，与 #6 重复 | 要（不一致=静默 `gateway:null`） |
| 11 | `admin_routes/mod.rs:110-115` | REST 路径 `/admin/api/claude-sessions*` | 看决策（改则破坏 `docs/admin-api.md:163-165`） |
| 12 | `admin_rpc.rs:458,468,479` | RPC 名 `claude_sessions_*` | 看决策（同步 `index.js:389,425,504`+smoke mock） |
| 13 | `summarize.rs:481-495`、`read.rs:176-206`、`paths.rs:66-79` | 子代理概念 `<id>/subagents/agent-*` | 要（其它来源无此概念） |
| 14 | `observe.rs:68-75` | `not_found_or_internal` 靠中文子串 `"不存在"`/`"无效的"` 判 404 | 要：新文案须沿用，否则 404 变 500 |

**不能松的三道边界**：①**先校验 id 再拼路径**——`paths.rs:42-43` 一次严格 UUID 拒掉 `..`/分隔符/绝对路径/symlink，`:88-104` 再有
`canonicalize`+`starts_with(&root)` 第二道；多来源时须由每个来源自带（钉死测试 `claude_sessions.rs:422-450`、`admin_api.rs:1043-1058`）。②**`open_session_in_browser` 只收 id、URL 由后端拼**（`admin_rpc.rs:491-521`；`desktop_hooks.rs:162-180` 校验 id、`:182-201` 的 `is_console_url` 复核前缀）——把来源拼进 id 会绕过单段校验。③#14 的中文子串约定。

**三个候选抽象面（选项，非决定）**：**A `trait SessionSource`**（`describe/ids_ok/candidates/read/prefix`，`Box<dyn>` 或泛型）——
最灵活；但 `candidates()` 现为同步 `Vec`，trait 签名会把「一来源=一次 walk」钉死。**B `enum SourceKind`+match**——改动最小、
穷尽性由编译器保证；但每加来源要回 `paths.rs`/`summarize.rs` 中心改，`match` 会渗进 `Accum`。**C 静态 registry**
（`&'static [&dyn Source]`+每来源一模块）——与 A 等价、编译期装配、无运行时依赖。三者共同前置：`Candidate` 是 `pub(crate)`，
改它不破坏 wire，**是最便宜的注入点**。

## 3. 二级聚合：能否只靠 stat

**实测（`~/.claude/projects`）**：顶层目录 13（**3 个空**：MiMo-Code、deepseek-code-public、dolt2qlib-workspace-rd-agent）／
有会话的 10／**会话 31**；总字节 56 941 510 = **54.3 MiB**（`du -sh` 报 72M，含 memory/）；单文件 最大 12 930 553、中位 404 447、
最小 2 922 B；>512 KiB（走 head+tail 分支）**13** 个（53.0 MiB）、≤512 KiB 18 个；单目录最多 10 个（`-Users-denis-Sites-proxy-rs`）；会话 id 跨目录重名 **0**。**⚠ 31 个会话的绝对耗时只是下界，不可当容量依据**，故同时给 300 文件合成 fixture。

**三个分组键候选都会出错**
①`encoded_project`：stat 阶段就有（`paths.rs:145`），零额外 IO；但编码有损——实测
`enc('/Users/denis/Sites/dolt2qlib/workspace/rd-agent') == enc('…/dolt2qlib-workspace-rd-agent') ==
'-Users-denis-Sites-dolt2qlib-workspace-rd-agent'`（True）。该目录本机真实存在（只有 `memory/`、无 jsonl），DSH 同形目录 `--…--`
下 2 个会话真实 `cwd` 均为多段路径形态 → **碰撞已发生，只是恰好只有一侧有数据**。`types.rs:41-43` 注释亦写明 `cwd` 为空时
**不**用目录名反推。
②transcript 的 `cwd`：准确可显示，实测 31/31 见头 256 KiB（首个 `cwd` 行偏移 min 188／中位 507／**max 52 491**）；只读 4 KiB 覆盖
30/31、32 KiB 才 31/31。但**同文件内会变**：`…quant-temperature/09fbd991-….jsonl` 在 `…/quant-temperature` ↔ `…/quant-temperature/server`间来回 7 次；`…deepseek-harness/6ec44865-….jsonl` 在 `…/deepseek-harness` ↔ `…/packages/llm/llm-pi-ai` 间来回 3 次 →
「会话属于哪个项目」**本身不是单值**。
③冲突时：实测 `encode(cwd)==dirname` **31/31**、每目录内 `cwd` 集合只有 1 值、**零冲突** → 本机数据**无法**揭示冲突规则，
须设计者定（取目录名／首个 cwd／末次 cwd／还是列「多目录会话」）。

**成本：stat-only 分组 vs 全量 head+tail**（Rust `--release`，源码 §7②）
```
真实 ~/.claude/projects : stat-only grouping: 31 files, 13 groups in 1.02 ms   （3 次中位 ~1.0 ms）
300 文件 fixture        : stat-only grouping: 300 files, 24 groups in 1.48 ms
真实数据 head+tail      : 2918 lines in 11.0 ms（5 次 range 11.0–15.6），实读 10.2 MiB / 磁盘 54.3 MiB
300 文件 head+tail      : 21600 lines in 96.3 ms（5 次 range 96.0–113.0）
```
stat-only 基准做的正好是「组内会话数 + 组 max(mtime) + 组 Σ(size)」，每文件一次 `metadata()`；Python 对照 `walk+stat = 0.4–0.6 ms`。
→ 分组额外成本 ≈ 现有 `candidates()` 成本（**1–1.5 ms/全量**），**不需要第二次 stat**（`paths.rs:140` 已拿到 `meta`）。
**关键**：`limit=50` 只对命中 50 行读文件（`read.rs:15-16`），故组级精确 `turns` 会把成本从 **O(limit) 变 O(total)**。
**最小触碰集**：`types.rs:229-234` 给 `Candidate` 加 `size_bytes:u64`（+来源字段若多来源）｜`paths.rs:140-148` 把已拿到的
`meta.len()` 塞进去（**零新 IO**）｜`read.rs:11-13` 分组须在 sort 前/同时做、`total` 语义要重定义（§4）｜`ops_sessions.rs:86-97`
新字段**必须**在此加，否则 wire 上不存在。

## 4. 分页 × 分组：契约层硬冲突

**今天的语义**：`total`=全量候选数（`read.rs:13`，排序后 skip 前）；`offset`/`limit` 是跨全部会话切片；`limit` `clamp(1,200)`
（`read.rs:9`，故 `limit=0`→1）；排序 `(mtime desc, id desc)`（`:12`）。前端三个依赖点：文案
`显示第 {offset+1}–{offset+shown} 个 · 共 {total} 个`（`list.js:152-154`）、上一页 `disabled=offset<=0`（`:158`）、
下一页 `disabled=offset+limit>=total`（`:161-163`）。

**冲突（推断）**：「组内分页+组级计数」要求组头 `count` 是该组**全量**数、分页在**组内**，而 `total`/`offset` 现在是全局的。
具体矛盾：加 `group=<key>` 后 `total` 是「组内总数」还是「全局总数」？前者让 `:161-163` 正确，但 `:152-154` 的「共 N 个」
会从全局数静默变成组内数——**同一字段两个含义**。

**兼容硬约束（实测）**：①唯一客户端读 `page.total/offset/limit/sessions`（`list.js:147-163`），只加字段、不改这四者语义是唯一无损路径；
②Rust 侧**无回归网**（`grep 'body\["total"\]\|body\["offset"\]\|body\["limit"\]' tests/admin_api.rs` → 空）；③加字段两道工
（结构体 + `json!`）；④serde 实测：`Option<T>` 缺失可反序列化、`flatten`+未知键不报错（默认不 `deny_unknown_fields`），
`types.rs` 全文件 **0 处 `serde(default)`** 但字段皆 `Option`，无需补；⑤`MAX_LIMIT=200` 的 clamp 对任何名为 `limit` 的新参数生效。

**建议 wire 契约（选项，非决定）**
```jsonc
{ "available":true,"reason":null,"root":"…","sources":[…],"stats_available":true,"gateway_error":null,
  "total":31,"offset":0,"limit":50,"sessions":[…],        // ← 旧字段，语义一字节不变
  "group_by":"project",                                   // "none"(缺省) | "project" | …
  "groups":[{"key":"/Users/denis/Sites/proxy-rs","label":"proxy-rs","count":10,
             "last_seen_ms":1794332000000,"size_bytes":8101888,"selected":true,
             "sessions":[ /* 与顶层同构的 SessionRow */ ]}],
  "groups_truncated":false }                              // 组内被 limit 截断时为 true
```
新参数（`ListArgs` 与 `ClaudeSessionsQuery` 同加，均 `Option`）：`group_by`（`None`=今天行为，**字节级不变**）、`group`
（过滤到该组；`None`=不过滤，`total` 仍全局）、`group_limit`（缺省=全部，受 `MAX_LIMIT`）。
**三条可验收不变量**：①不带新参数时 `sessions/total/offset/limit` 与 3.5.5 完全一致（含排序）；②不带新参数时响应
**不出现** `groups`（`skip_serializing_if`）或明确为 `null` —— **须二选一**，因 `smoke/fixtures.mjs:78-107` 的 mock 逐字段比对；
③`limit` clamp 语义对新参数一致。**代价**：`groups[].count` 只能来自 stat 聚合（§3），**不含**精确 `turns`。

## 5. DSH 与 Codex 的真实格式

**DSH 布局**：`~/.dsh/sessions/--<cwd.strip('/').replace('/','-')>--/<session-<uuid> 或 裸 <uuid>>/session.vN.jsonl.zstd`（`session.lock` 388 个均 0 B，不是会话）。实测：项目目录 19／会话目录 **389**／文件 784（`.zstd` **396** 占全部字节）；压缩
**545 386 508 = 520.1 MiB**（`du -sk` 533 584 KiB）；**解压 1 909 029 911 = 1820 MiB**（3.2–4.0x）；单文件最大 18 641 792 B（解压 59.2 MiB）；文件名版本 `v4` 280／`v3` 113／`v2` 1／无版本 2（首行 `version`：4:279、3:107、2:1、0:2）；id 形状
`session-<uuid>` **228**／裸 `<uuid>` **161**（161 个裸的全是 `origin:"subagent"` 且有 `parentSession`）；目录名==首行 `id`
**389/389**、`encode(cwd)==dirname` **389/389**、有 `session.vN` 的会话 389（无遗漏无多余）。⚠ **228/389 的 id 过不了
`is_valid_session_id`**（长度非 36），接进现有链路会被 `paths.rs:137` 的 `continue` **静默丢弃**。

**DSH 解码成本（实测）**：`find … | xargs cat | zstd -dc | wc -c` → 1909029911，`real 2.99s`；396 文件分别解码取首行
（read 4096 后 kill）**936 ms**（2.4 ms/文件），每文件完整解码后取首行（最坏）**4345 ms**。`zstd` CLI 在 `/opt/homebrew/bin/zstd`
（v1.5.7，**不在默认 PATH**）；`python3 -c "import zstandard"` → `ModuleNotFoundError`；magic `28 b5 2f fd` 为标准帧（非 skippable）→ 必须真解码才能读到第一行。

**DSH 解析差异：`summarize.rs` 整体不可复用**。首行自带元数据即可撑起列表行（`{"type":"session","version":4,"id":"14c23a6e-…",
"cwd":"/Users/denis/Sites/datalake","parentSession":"session-…","origin":"subagent"}`）；后续是事件流
`{"type":…,"seq":N,"time":<epoch_ms>,"data":{…}}`（`subagent/descriptor`、`sandbox/mode`、`approval/policy`…）。Claude 的`user/assistant/ai-title/cost-state` **一个都不出现**；时间字段是 `time`(epoch ms) 而非 `timestamp`(ISO)，故 `parse.rs:117-121` 的 `ts_of` 也失效。**但 DSH 有第二个元数据源、不经解压**：`~/.dsh/storages/session_projcache/sessions/<id>.json`。实测 389 文件 /
12.9 MiB、解析 **53 ms**、与 389 个会话目录**一一对应（缺 0）**、`identity.cwd` **389/389** 都有；`rows.title.val` 标题
（**28 空**）、`rows.sessionListMetadata.val.lastPromptAt` 最后活动（**54 缺失**）、`rows.sessionStats.val` 含 turns/steps/llmMs；`lastPromptAt` 范围 `1786970472392..1794304710303`。⚠ **它≠文件 mtime**：335 个可比样本 `mtime >= lastPromptAt` **335/335**，中位差 935 s、最大差 4 814 669 s（≈56 天）→ 两源排序不同；读 projcache（53 ms，字段可能缺）vs 解压首行（936 ms，字段全）**差 ~18x**。

**Codex 布局**：`~/.codex/sessions/YYYY/MM/DD/rollout-<ISO时间>-<uuid>.jsonl.zst`（恢复会话为
`rollout-…-<uuid>_<resume_uuid>.jsonl.zst`）+ `~/.codex/archived_sessions/`。实测 `sessions` 13 文件/1 974 487 B、`archived_sessions` 1 文件/8 538 B、单文件最大 844 354 B、解压总量（14 文件）17 796 447 B = 17.0 MiB（`real 0.01s`）；文件名 uuid 可解析 13/13 且
**13/13 都能过 `is_valid_session_id`**；与 Claude/DSH 的 id 交集 **0**。首行 `{"timestamp":…,"ordinal":0,"type":"session_meta","payload":{"session_id":…,"cwd":…,"source":"vscode","git":{…}}}`。**两处结构差异**：①目录按日期**不按项目** → 分组键必须解压
首行取 `payload.cwd`（O(total) 而非 O(1)）；②**双 uuid 陷阱**：`rollout-2026-09-27T16-14-35-01a0e0b1-…_01a0e1ee-….jsonl.zst` 的首行 `session_id` 是**前一个**（`01a0e0b1-…`），哪个是主键需明确规则，`is_valid_session_id` 无法区分。

**为什么 3.5.6 不该真读**（全部本机实测）

| | Claude Code | DSH | Codex |
|---|---|---|---|
| 压缩 | 否 | **是（5 个版本号）** | **是** |
| 本机压缩/解压 | 54.3 / 54.3 MiB | **520 / 1820 MiB** | 1.9 / 17.0 MiB |
| 列表最小 IO | 10.2 MiB / **11 ms** | 全量 **2.99 s**；首行 **936 ms** | 17.0 MiB / ~10 ms |
| 新依赖 | 无 | **`zstd`（`Cargo.toml`、`Cargo.lock` 实测均无）** | 同左 |
| 分组键来源 | 目录名（stat） | 目录名（stat，389/389 正确） | **须解压首行** |
| id 过现有校验 | 31/31 | **161/389** | 13/13 |
| `summarize.rs` 可复用 | — | **否** | **否** |

最硬一条：加 `zstd` = 往纯 Rust 构建链插 C 库（`zstd-sys`）；520/1820 MiB 与 54 MiB 差 9.6–34 倍，直接违反
`claude_sessions.rs:17-23`「整个请求只包一次 `spawn_blocking`」的前提（一次 3 s 阻塞会打满小共享线程池）。

## 6. 现有测试钉死了什么

**`tests/admin_api.rs:989-1074` 的 4 个会话测试**：`write_claude_fixture`(helper `:989-1007`)；`claude_sessions_lists_local_transcripts_with_source_descriptors`(`:1010-1041`)：`available==true`、`sessions[0].id/cwd=="…/proxy-rs"/title=="hello"`、**`sources[0].kind=="claude_local"`、`sources[1].kind=="dsh_local"`、`available==false`、`note` 非空**、RPC detail 的 `result.timeline` 长 2；`a_path_in_the_session_id_is_not_a_path`(`:1043-1058`)：三条 URL（`..%2F..%2Fetc%2Fpasswd`、`..`、`etc/passwd`）必非 200；`claude_sessions_without_a_source_explains_itself`(`:1060-1071`)：`available==false` 且 `reason` 含 `".claude/projects"`。另 `:310-331`（两条 claude-sessions 路由匿名 401 且 `error.type=="unauthorized"`）、`:724-747`（`claude_sessions_list` 的 `result` 非 null）。

**`claude_sessions.rs:91-572`（17 个 `#[test]`）中钉住 list 形状/分页的**：`:174-191` `list_finds_uuid_files_only`（`page.total==1`，`not-a-session.jsonl` 与 `memory/MEMORY.md` 被排除）；`:193-204` `cwd_comes_from_the_line_not_the_directory_name`（目录名 `-Users-a-b-c` 但 `cwd` 取行内 → **直接限制分组键不可回退目录名**）；`:206-230` 标题取最后一个 `ai-title` 否则首个人类 prompt；`:352-374`
`head_tail_listing_uses_tail_for_title`（title 来自 tail、`counts_partial==true`、cwd 来自 head）；`:376-419` `timeline_pagination_is_stable`（**是 timeline 分页非 list**：offset 0 为最新页、`timeline_total==10`、`has_more`）；`:421-450` `invalid_ids_are_rejected`；`:519-571` `gateway_join_uses_the_claude_namespace`（**只有 `claude:<uuid>` 能 join**、未知 id 的 `gateway` 是 `None` 非 0）。
⚠ **list 分页无单测**：全文件 `ListArgs` 只用 `::default()`，从不设 `offset`/`limit`；`read.rs:9-15` 只由 `admin_api.rs:1018` 一次不传参请求覆盖。

**`ui/tools/smoke` 会破的具体值**：`scenarios-views.mjs:115` `.sess-row` 数 == **1**（组头若也用该 class → 2）｜`:116` 首行标题 == `'会话页测试'`｜`:117` 首行有 `.sess-badge`｜`:118` `[data-kind="dsh_local"].disabled === true`（**DSH 一旦 `available:true` 必破**）｜`:119-124` 点行调 detail、timeline 2 条、toolName `Bash`｜`:129-132` 每行恰 1 个 `.sess-row-open`、`data-open-id` 正确、点击只调 `open_session_in_browser` 且**不**触发 detail｜`:252-258` hash：`#logs`/`#settings`/未知→`overview`/`#sessions/<uuid>` 保 id 并选中该行/坏 id 仍解析到 sessions｜`scenarios-states.mjs:123-125` 空列表：文案含「没有找到」、`.sess-row` 数 **0**｜`fixtures.mjs:78-107` `claude_sessions_list` mock **逐字段**（`total:1,offset:0,limit:50,sessions:[{…16 字段…}]`）—— 加 wire 字段须同步，漏一个不是失败而是「mock 与真实偏离」（`ui/README.md:180-186` 专此警告）｜
`fixtures.mjs:146` open 返回 `url:'http://127.0.0.1:3457/admin/#sessions/<uuid>'`。

**必须保留的向后兼容点**：①**`#sessions/<uuid>` 形状**，三个独立校验点须同步（`ui/src/main.js` 的 tab 段、`index.js:50` 的 `SESSION_ID_RE`
+`:453-458`、`admin_routes/login.rs:58-64,76-98` 的 `landing_path`+`next_hash`），测试 `session_open_url.rs:228-320`（含列 10 种必拒形态的 `the_deep_link_allow_list_is_not_an_open_redirect`）；②`open_session_in_browser` 只收 id（`admin_rpc.rs:491-521`→`desktop_hooks.rs:162-180`→`:182-201` 复核），测试 `session_open_url.rs:133-226,391-483`（含 `the_rpc_ignores_client_supplied_url_host_and_port`）；③`sources[1].kind=="dsh_local"` 且 `available==false`（`admin_api.rs:1028-1033`）；④gateway join key 必须 `claude:<uuid>`；⑤不带新参数时列表字节级不变（含排序）。**静默失败（无任何测试会红）**：`ListPage` 加必填字段却不改 `ops_sessions.rs` 的 `json!`。

## 7. 复现命令

§3 的 Rust 基准在 `/tmp`（**不碰仓库**）：`/tmp/bench-io` 唯一依赖 `serde_json="1"`，`src/main.rs` 逐句镜像
`summarize.rs:428-479` 的 head+tail 读 + `from_str`，`src/bin/group.rs` 做 stat-only 分组；`/tmp/claude-bench` 是把一个
404 447 B 真实 transcript 复制 300 份到 `proj-0..23`。

```bash
# ① §3 Claude 目录/文件/字节 + walk+stat（输出：dirs 13 files 31 bytes 56941510 … ms 0.63）
python3 -c "import os,time;r=os.path.expanduser('~/.claude/projects');t=time.perf_counter();d=[x for x in os.listdir(r) if os.path.isdir(r+'/'+x)];f=[os.stat(r+'/'+x+'/'+n).st_size for x in d for n in os.listdir(r+'/'+x) if n.endswith('.jsonl')];print('dirs',len(d),'files',len(f),'bytes',sum(f),'ms',round((time.perf_counter()-t)*1000,2),'max/med/min',max(f),sorted(f)[len(f)//2],min(f),'>512K',sum(1 for y in f if y>524288))"
# ② §3 Rust 基准
export PATH=$HOME/.cargo/bin:/opt/homebrew/bin:$PATH; cd /tmp/bench-io && cargo build --release --offline
./target/release/bench-io ~/.claude/projects ~/.claude/projects  # 0.26–0.97ms / 11.0–15.6ms
./target/release/bench-io /tmp/claude-bench /tmp/claude-bench    # ~1.1ms / 96–113ms
./target/release/group ~/.claude/projects                        # 31 files, 13 groups, ~1.0ms
./target/release/group /tmp/claude-bench                         # 300 files, 24 groups, ~1.5ms
# ③ §5 zstd（CLI 在 /opt/homebrew/bin；python3 无 zstandard 模块）
xxd -l 32 ~/.dsh/sessions/*/*/session.v4.jsonl.zstd | head -2                       # 28 b5 2f fd
find ~/.dsh/sessions -name "*.jsonl.zstd" -print0 | xargs -0 cat | zstd -dc | wc -c # 1909029911 / real 2.99s
find ~/.codex/sessions ~/.codex/archived_sessions -name "*.jsonl.zst" -print0 \
  | xargs -0 cat | zstd -dc | wc -c                                                 # 17796447
# ④ §3.2 编码碰撞 → True
python3 -c "e=lambda c:c.replace('/','-');print(e('/Users/denis/Sites/dolt2qlib/workspace/rd-agent')==e('/Users/denis/Sites/dolt2qlib-workspace-rd-agent'))"
# ⑤ §3.2 cwd 中途变化 → 命中 2 个
python3 -c "
import glob,json,os
for fp in glob.glob(os.path.expanduser('~/.claude/projects/*/*.jsonl')):
    if os.path.getsize(fp)>3_000_000: continue
    cw=set()
    for l in open(fp,'rb'):
        try: v=json.loads(l)
        except Exception: continue
        if isinstance(v,dict) and isinstance(v.get('cwd'),str) and v['cwd']: cw.add(v['cwd'])
    if len(cw)>1: print(fp,cw)"
# ⑥ §5 DSH 覆盖度：zstd 计数 / projcache 字段
python3 -c "import os,collections;r=os.path.expanduser('~/.dsh/sessions');n=[s+'/'+f for x in os.listdir(r) for p in [r+'/'+x] if os.path.isdir(p) for s in [p+'/'+y for y in os.listdir(p)] if os.path.isdir(s) for f in os.listdir(s) if f.endswith('.zstd')];print(len(n),sum(os.path.getsize(x) for x in n),collections.Counter(os.path.basename(x) for x in n))"
python3 -c "
import os,json,glob,time
r=os.path.expanduser('~/.dsh/storages/session_projcache/sessions');fs=sorted(glob.glob(r+'/*.json'));t=time.perf_counter();c=l=ti=b=0
for f in fs:
    rec=(json.load(open(f)).get('record') or {});i=rec.get('identity') or {};ro=rec.get('rows') or {};s=(ro.get('sessionListMetadata') or {}).get('val') or {}
    c+=bool(i.get('cwd'));l+=bool(s.get('lastPromptAt'));b+=bool(s.get('blank'));ti+=bool((ro.get('title') or {}).get('val'))
print(len(fs),'files',round((time.perf_counter()-t)*1000),'ms cwd',c,'lastPromptAt',l,'title',ti,'blank',b)"
```
serde 行为用 `/tmp/serde-t`（`serde`+`serde_json`）实测：`Option` 缺失可反序列化 = true、`flatten`+未知键不报错 = true。

## 8. 速查：改动爆炸半径

| 若要做 | 后端必改 | 会破的测试 | 新依赖 |
|---|---|---|---|
| **分组（stat 级）** | `types.rs:229-234`(Candidate+size)、`paths.rs:140-148`、`read.rs:11-13`、`ops_sessions.rs:86-97` | 无（旧字段语义不变即可） | 否 |
| **分组 + 组内分页** | 上 + `types.rs:168-177`、`observe.rs:120-124`（`admin_rpc.rs:458` 的 `from_value` 自动跟随） | `list.js:147-163` 的字段语义（**无 Rust 测试**） | 否 |
| **来源可插拔（不接新源）** | `types.rs:6`、`paths.rs:10-12,26-37,44-64,82-107,111-152`、`summarize.rs:62-96`、`ops_sessions.rs:9-19,79,107,114,128`、`observe.rs:68-75` | `admin_api.rs:1010-1071` 的 `sources[]` 顺序与内容 | 否 |
| **真接 DSH** | 上 + `Cargo.toml`(+zstd) + 新解析器 + `desktop_hooks.rs:166` id 校验放宽 | `admin_api.rs:1031`、`scenarios-views.mjs:118` | **是（zstd/zstd-sys，C 库）** |
| **真接 Codex** | 上 + 按日期分层 walk + 双 uuid 规则 + 解压取 `payload.cwd` 分组 | `scenarios-views.mjs:115-118`（fixture 只有 claude_local） | **是（zstd）** |
| **带来源的会话地址** | `admin_routes/login.rs:76-98`、`desktop_hooks.rs:162-201`、`index.js:50,453-458`、`ui/src/main.js` | `session_open_url.rs:228-320,415-483`、`scenarios-views.mjs:252-258` | 否 |
