# proxy-rs 3.3.0 规划：提高内聚、降低耦合

> 目标版本：**3.3.0**（当前 3.2.2，commit `76230dd`）
> 一句话目标：**把「一个功能要实现四遍」变回「一遍」**，并把 7 个模块依赖环清零。
>
> 本文所有数字与行号均取自 `git archive HEAD`（`76230dd`）的提交快照，可复现：
> `rm -rf /tmp/b && mkdir /tmp/b && git archive HEAD | tar -x -C /tmp/b`。
>
> 上游输入：`docs/audit-3.3.0/module-graph.md`（结构审计，1207 行）、
> 本文 §2 的管理面漂移清单（Lead 实测）。

---

## 1. 现状：三个可度量的问题

| # | 症状 | 实测证据 | 代价 |
|---|---|---|---|
| **P1** | 同一份逻辑写两遍（桌面 / 服务器） | 4 组函数重复实现（1 组字节相同、3 组语义等价）；54 个 Tauri 命令里 48 个在 `admin_rpc` 被重新实现 | 改一处要记得改两处；漏了就是静默漂移 |
| **P2** | 7 个模块依赖环 / 3 个强连通分量 | `admin ↔ admin_routes`、`client_accounts ↔ config`、`models → stats → … → models` | 无法单独测试、无法抽出子 crate、改动半径不可控 |
| **P3** | 三个 god module 名不副实 | `workbuddy_auth` 8 件事 / 2515 行；`settings` 4 件事 / 557 行；`util` 4 件事 / 428 行 | 建模块时无法预测该放哪、找代码靠 grep |

### 1.1 P1 的代价已经变成了线上 bug

这不是理论风险。3.2.2 里已经出现三处由「两遍实现」直接导致的缺陷（§2 逐条给证据）：

- **服务器端切换「入站鉴权」开关不生效**（已实测复现）
- **桌面端点「重新签发 key」按钮直接报错**（命令根本没注册）
- **掩码 key 的判定规则两边不一致**（一边防 `*`、一边不防）

---

## 2. P1：管理面重复与漂移（Lead 实测，可复现）

### 2.1 重复实现（4 组）

**先说准确的程度**：这 4 组**不是逐字相同**。实测只有 1 组字节级一致，
其余 3 组是**语义等价** —— 输出的 JSON 键集合完全相同、逻辑分支相同，
但函数签名、局部变量名、`format!` 写法有差异。

| 桌面端 | 服务器端 | 字节相同 | JSON 键集合相同 | 非注释差异行 |
|---|---|---:|---:|---:|
| `src-tauri/src/main.rs:247 session_metrics_json` | `src/admin_rpc.rs:375 session_metrics_json` | **是** | 是 | 0 |
| `src-tauri/src/main.rs:2291 mask_key` | `src/admin.rs:884 mask_key` | 否 | 是 | 4 |
| `src-tauri/src/main.rs:600 mask_credential` | `src/admin.rs:231 credential_json` | 否 | 是 | 6 |
| `src/api.rs:346 account_json` | `src/admin.rs:605 client_account_json` | 否 | 是 | 18 |

**判定方法**（可复现）：提取函数体 → 去注释/空行 → 把 `proxy_rs::` 与 `crate::` 前缀抹平
→ `difflib` 比对；同时用正则抽取 `"key":` 形式比对 JSON 键集合。
脚本见本节末。差异行数 = `difflib` 中 `+`/`-` 行数。

**这仍然是要合并的重复，理由不是「字面一样」而是三条**：

1. **JSON 键集合逐个相同** —— 这正是消费方（`ui/`）依赖的契约。
   任何一处字段增删漏改一边，同一个页面在不同部署上就会显示不同数据；
   而两边跑的是**同一份 `ui/`**，所以症状是「同一个页面在不同部署上不一样」，
   是最难排查的一类。**契约相同而实现分家，比两份契约不同更危险** ——
   因为没有任何地方会因为不一致而报错。
2. **`mask_key` 的 4 行差异是纯写法差异**（`format!("{head}••••{tail}")` vs
   `format!("{}••••{}", head, tail)`），合并时取任一种即可，零语义风险。
3. **`account_json` 的 18 行差异是变量名 `a` vs `account`** —— 差异行数大但不含逻辑差异。

**复现脚本**：

```python
# 提取函数体 → 去注释 → 抹平路径前缀 → difflib + JSON 键集合比对
def body(path, fn):
    src = open(path).read().splitlines()
    for i, l in enumerate(src):
        if re.match(r'^(pub )?(async )?fn ' + fn + r'\b', l):
            for j in range(i, len(src)):
                if src[j] == '}': return src[i:j+1]

def norm(lines):
    out = []
    for l in lines:
        s = l.strip()
        if not s or s.startswith('//'): continue
        out.append(s.replace('proxy_rs::', '').replace('crate::', ''))
    return out
# 然后比对 norm(body(...)) 与 JSON 键集合 sorted(re.findall(r'"([a-z_]+)":', ...))
```

> **写这一节时我犯过一次错**：初稿写成「函数体逐字相同…diff 只有 2 个 hunk」，
> 那是只看了 `credential_json` 一组就推广到全部 4 组。实测后改正。
> 结论方向不变（该合并），但**证据强度降低了** —— 3 组需要人工确认语义等价，
> 不能机械地「删除重复的那份」。

### 2.2 行为漂移（真 bug，3 条）

#### BUG-1：服务器端「入站鉴权」开关保存后不生效（已复现）

**证据链**：

- 共享 UI 里有这个开关：`ui/src/views/settings/save.js:35`（`client_auth_enabled`）
- 服务器端保存路径 `src/admin.rs:173 save_settings` **只写文件**，没有触碰运行中的状态
- 桌面端保存路径 `src-tauri/src/main.rs:555` 保存后紧接着
  `ctx.client_auth.set_enabled(auth_enabled)` + `reload_from_store().await`
- 全库搜索：**服务器端没有任何一处调用 `ClientAuthState::set_enabled`**
- 守卫读的是内存态：`src/client_auth.rs:113 if !state.is_enabled()`
- `ClientAuthState::new(...)` 只在进程启动时构造一次（`src/bin/proxy-rs-server.rs:180`）

**实测复现**（Lead 用 `examples/` 探针跑过，探针已删除）：

```
settings file says:  true      ← 文件已改成开启
live guard says:      false     ← 运行中的守卫仍是关闭
data plane gate open? true      ← 数据面依然不校验 key
```

**用户可见后果**：在 web 控制台勾上「入站鉴权」→ 点保存 → 提示成功 →
但网关继续接受无 key 请求，直到 `systemctl restart proxy-rs`。
**这是一个安全开关假装生效**，比没有这个开关更危险。

**归属**：P1（重复实现导致的能力缺口）。修法是让两端共用同一段「保存后热应用」逻辑，
而不是在服务器端补一遍 —— 补一遍就会出现第三份实现。

#### BUG-2：桌面端「重新签发 key」按钮必然失败

**证据链**：

- UI 按钮无条件渲染：`ui/src/views/settings/client-accounts.js:327-329`（`data-client-account-reissue`）
- 点击时调用：`ui/src/views/settings/client-accounts.js:365 invoke('client_account_reissue', {id})`
- 服务器端有实现：`src/admin_rpc.rs:279 "client_account_reissue" =>`
- **桌面端没有这个命令**：`src-tauri/src/main.rs` 里 `client_account_reissue` 出现 **0 次**
  （既无 `#[tauri::command]` 定义，也不在 `invoke_handler` 注册表 `:2762` 附近）
- 而它的三个兄弟命令都注册了：`client_account_create` / `client_account_toggle` / `client_account_delete`

**用户可见后果**：桌面端点「重新签发」→ Tauri 抛
`command client_account_reissue not found` → toast 显示「重新签发失败: ...」。

**归属**：P1。这是个**单向漂移**：命令是为服务器端需求加的（服务器没有原生保存对话框，
必须重新签发才能拿到明文 key 写进客户端配置），加的时候忘了桌面端也要一个。

#### BUG-3：掩码 key 判定规则不一致

- 桌面端 `src-tauri/src/main.rs:496 resolve_api_key` 拒绝 `•` **和** `*` 两种掩码字符
- 服务器端 `src/admin.rs:186` 只拒绝 `•`

**可达性判定**：目前**不可达** —— 全库没有任何代码产生 `*` 形态的 key 掩码
（`mask_key`/`mask_secret` 都只产生 `•`；`*` 只出现在 `main.rs:2817` 的**测试断言**里）。
所以这是**防御性不一致**，不是活 bug。

**但仍应统一**：这条规则的存在本身就说明历史上有人担心过 `*` 掩码。
两处规则不同 = 下次谁改了一处就成真 bug。归入「加一致性测试」而不是「修 bug」。

### 2.3 结构性重复：48 个命令写两遍

| 项 | 数量 |
|---|---|
| Tauri `#[tauri::command]` | 54 |
| `admin_rpc::dispatch` 复刻的命令名 | 48 |
| **两者共享（同一命令名两份实现）** | **47** |
| 桌面端独有的（合理，服务器无法实现） | 7：`start_service`、`open_logs_dir`、`reveal_path`、`api_keys_set_default`、`wb_set_default`、`wb_credentials_add`、`wb_growth_preview` |
| 服务器独有的 | 1：`client_account_reissue`（= BUG-2） |

> `stop_service` **不在**桌面独有列表里：服务器端有它的实现（`src/admin_rpc.rs`），
> 只是会礼貌拒绝并提示用 `systemctl`。这是**正确的做法** —— 明确说"做不到"，
> 而不是假装成功。`start_service` 才是真的两边都没有。

**这 47 个不是「两套接口」，是「一套语义、两份实现」。**
`ui/` 是同一份，`invoke` 的 wire 契约也特意保持命令名一致（`ui/server/bridge.js` 的设计），
但真正的业务逻辑在两边各写了一遍。

**注意 `admin_rpc::dispatch` 已经是最好的拆解样本**：它的 48 个分支里绝大多数只是一行
`admin::xxx(...)` 转调，真正重复的是被它转调的那些 `admin::` 函数本身
（它们与桌面端同名函数重复）。所以 P1 的修法是**把被转调的那一层变成共享层**，
而不是再去抽象 dispatch。

### 2.4 API 面还有第三、第四套

| 面 | 位置 | 命令/路由数 | 消费者 |
|---|---|---|---|
| 数据面 admin | `src/api.rs:346-534` | 4 个 handler（`/api/admin/client-accounts*`） | 只有 `tests/client_auth_admin.rs` |
| 服务器 REST | `src/admin_routes.rs` | 31 条 `.route()` | `docs/admin-api.md` 承诺给 curl 用；实测**无脚本消费者** |
| 服务器 RPC | `src/admin_rpc.rs` | 48 个命令 | web 控制台（经 bridge） |
| 桌面 IPC | `src-tauri/src/main.rs` | 54 个命令 | 桌面窗口 |

**同一个 `client_account_json` 逻辑在 3 个地方各有一份**
（`src/api.rs:346`、`src/admin.rs:605`、以及桌面端通过 `admin.rs` 的转调）。

`src/admin_routes.rs` 31 条 REST 路由的测试覆盖实测：

```
完全无测试：stats / request-logs / session-metrics / providers / models
            credentials / preferences / growth / scheduler
有测试：   status(5) settings(3) identities(2) keys(2) client-accounts(3) logs(1) checkin(1) pool(1)
```

**约一半的 REST 面没有任何测试，且没有消费者。** 这是 P1 里最该瘦身的部分。

---

## 3. P2：7 个模块依赖环

完整分析见 `docs/audit-3.3.0/module-graph.md` §1。摘要：

| 环 | 形态 | 根因 |
|---|---|---|
| #1 | `admin ↔ admin_routes` | `admin.rs` 三个函数签名收 `Arc<admin_routes::AdminState>` |
| #2 | `client_accounts ↔ config` | 只为读一个环境变量 / 一个默认值折算 |
| #3 | `api_token → config → client_accounts → api_token` | 同上 + 一个字节比较 |
| #4 | `config → workbuddy_auth → credits → config` | 积分协议常量放错了宿主 |
| #5 | `credits → session_pool → workbuddy_auth → credits` | 同上 |
| #6 | `client_accounts → workbuddy_auth → … → client_accounts` | 一个共享的泛型文件健康检查 |
| #7 | `models → stats → settings → providers → translate → models` | **唯一跨「纯核心 / I/O 外壳」的环** |

**关键结论：7 个环里 6 个可以用「搬函数 + 删一个死字段」解决，测试改动合计 4 行。**
只有 #7 需要真正重新分配职责。

**一个纯减法发现**：`src/config.rs:343` 是**没有消费者的计算** ——
`Config::client_auth_enabled` 全库只写不读（Lead 已复核：真正的开关在
`ClientAuthState`，两处调用者各自调 `enabled_from` 绕过 `Config`）。删掉即消环 #6 的最后一条腿。

---

## 4. P3：三个 god module 的病灶不同

| 模块 | 生产行 | 扇入 | 病 | 处方 |
|---|---:|---:|---|---|
| `workbuddy_auth` | 2515 | 11 | **8 件事共用一个名字** | 拆 8 个子文件 |
| `settings` | 557 | 19 | **4 件无关的事共用一个名字** | 拆 4 个子文件 |
| `util` | 428 | 18 | **名义扇入大，语义是单一的** | 只拆 HTTP/axum 相关两组，别动时间函数 |
| `stats` | 1528 | — | **不是 god module** | 只做 `TokenRecord` 下沉，其余不动 |

**为什么 `stats` 不该拆**：它只有一件事（请求日志的持久化与查询），
767 行的 `StatsDb` 全部方法操同一张 `request_logs` 表，是真实的内聚单元。
按「查询/写入」硬切会造出两个争抢同一个 `Connection` 池的模块，反而增加耦合。

**判据不是行数，是「是否有多件事被塞进一个命名」。**

`workbuddy_auth` 里最能说明问题的一组数字：

- W2/W5/W6/W7/W8（解析 + 刷新 + OAuth + 积分 + 打卡）合计 **1280 行**，服务 **0–5 个**库内模块
- W1/W3/W4（类型 + key 池 + 账号池）合计 **1290 行**，服务 **6–9 个**模块

**行数几乎相等，扇入差一个数量级。** 这就是它成为 god module 的数学原因。
其中 `oauth`（308 行）在库内**零消费方** —— 只有桌面端用（服务器端明确不支持扫码：
浏览器回调只能指向 `localhost`）。这一刀可以把 `qrcode` 从 headless 二进制的依赖里摘掉。

---

## 5. 目标：3.3.0 做什么，不做什么

### 5.1 做（按依赖顺序，每步可独立合入、独立验证）

| 步 | 内容 | 消环 | 测试改动 | 风险 |
|---|---|---:|---:|---:|
| **S1** | 建共享层 `src/admin_ops/`：把 §2.1 的 4 组重复函数收成一份，两端都调它 | — | 0 | 低 |
| **S2** | 修 BUG-1：把「保存后热应用」抽成共享函数，两端调用 | — | +2 测试 | 低 |
| **S3** | 修 BUG-2：桌面端补 `client_account_reissue` 命令并注册 | — | +1 守卫测试 | 零 |
| **S4** | 新增 `env.rs`（环境变量读取），4 个消费点改指向 | **-2** | 0 | 零 |
| **S5** | 新增 `store.rs`（`StoreState` + 泛型健康检查） | -1 条边 | 0 | 零 |
| **S6** | 新增 `billing.rs`（积分协议），从 `credits` 迁出 | **-2** | 0 | 零 |
| **S7** | 删 `Config::client_auth_enabled` 死字段 | **-1（环#6 收口）** | 4 行 | 低 |
| **S8** | 新增 `admin_state.rs`，`AdminState` 迁出 `admin_routes` | **-1** | 0 | 零 |
| **S9** | `settings` 拆 4 子文件（`paths`/`logbuf`/`logfile`/`gui`） | — | 0 | 低 |
| **S10** | `workbuddy_auth` 拆 8 子文件 | — | 0 | 中 |

**S1–S3 是 bug 修复，S4–S10 是结构重构。S1 必须最先做** ——
否则 S2/S3 会在第二份实现上再摞一层。

### 5.2 不做（明确排除，附理由）

| 不做 | 理由 |
|---|---|
| 继续拆 `src/proxy.rs`（`forward_request` 851 行） | 需要**新写测试**证明核心路径行为未变，风险与 3.3.0 的主题（降耦合）不同量级，留给 3.4.0 |
| `translate` 纯化（解除对 `util` 时钟依赖） | 要改纯函数签名，re-export 救不了，会动 `pipeline` 的测试 |
| `oauth` 移出库到 `src-tauri` | 收益（二进制体积）未实测，不做定量承诺；且要先确认 `qrcode` 真的能摘 |
| 给 31 条 REST 路由补测试 | 先决定这个面是否保留（§6），否则是给待删代码写测试 |
| 降低 `settings::paths` 的扇入（17） | 这是**健康的宽耦合**：纯 `PathBuf` 计算、无状态、无 I/O。压它属于过度设计 |

### 5.3 验收标准

1. `cargo clippy --all-targets -- -D warnings` 零告警
2. `cargo test` 全绿，且**测试总数只增不减**（当前 704）
3. 环从 **7 → 0**（或 ≤1，若环#7 分两步）
4. `docs/audit-3.3.0/module-graph.md` §6 的复现脚本重跑，环数为 0
5. §2.1 的 4 组重复函数在仓库中**只剩一份定义**
6. BUG-1/BUG-2 各有回归测试，且**能在修复前失败**

---

## 6. 三个需要你拍板的决策

这三个不定，S1 和 §5.2 的部分工作会白做。

### 决策 A：`/admin/api/*` REST 面留不留？

- **现状**：31 条路由，约一半无测试，**实测无脚本消费者**；`docs/admin-api.md` 承诺给 curl 用
- **选项 1（推荐）**：保留，但在 S1 里让它与 RPC **共用同一份 handler 实现**，
  并给「有路由无测试」的项补最小冒烟测试
- **选项 2**：砍掉 REST 面，只留 RPC + `docs/admin-api.md` 改写
- **影响**：选 1 增加约 31 个测试；选 2 删约 900 行但破坏文档承诺的 curl 用法

### 决策 B：`admin_rpc` 的 48 个命令要不要也收敛成共享层？

- **选项 1（推荐，= S1）**：把 `admin.rs` 里被转调的函数上移到共享层，
  `admin_rpc` 退化为薄 dispatch（它已经是了），桌面端命令体也只留下「取参数 + 调共享层」
- **选项 2**：只抽 4 组纯函数（`mask_key`/`credential_json`/`account_json`/`session_metrics_json`），
  业务逻辑维持两份
- **影响**：选项 1 一次解决 P1 的根，改动面大（两边各 ~40 个函数体变薄）；
  选项 2 只消除「显示层漂移」，BUG-1 这类**行为**漂移还会再出现

### 决策 C：`workbuddy_auth` 拆 8 个子文件，一次拆完还是分批？

- **选项 1（推荐）**：一次拆完。全部靠 `mod.rs` 的 `pub use` 兼容层做到**零测试改动**，
  分两次反而要在中间态维护两套路径
- **选项 2**：先拆 `oauth` + `refresh` + `points` + `parse` 这四块（1280 行、低扇入、边界清晰），
  `credential`/`store`/`api_keys` 留到 3.4.0
- **影响**：选项 1 单次 PR 较大（纯移动 + re-export，diff 噪音大但逻辑零变化）；
  选项 2 PR 小但 3.3.0 结束时 `workbuddy_auth` 仍是 4 件事的模块

---

## 7. 风险与缓解

| 风险 | 缓解 |
|---|---|
| 拆分时 `use super::*` 失效（624 个内联测试依赖它） | `mod.rs` 兼容层用 `pub use`；**注意** `src/proxy.rs:30-35` 那种同级文件测试必须用 `pub(crate) use` —— 这个区别是最容易踩的坑 |
| 大 diff 掩盖真实逻辑改动 | 每步只做「移动 + re-export」，**零逻辑改动**；用 `git diff -M --stat` 确认全是 rename |
| 服务器端和桌面端再次漂移 | S1 之后加一条**结构守卫测试**：断言两端调用的共享函数名一致（类似 §2.3 的命令名对照） |
| `claude_sessions`（并发会话正在开发）与本次重构冲突 | 该模块只依赖下层，不引入新环（audit 已复核）；S9/S10 拆 `settings`/`workbuddy_auth` 时它会同时被触及，需协调 |

---

## 8. 一页结论

1. **3.3.0 的主角是 P1（重复实现）而不是 P2（依赖环）。** 环是结构瑕疵，
   而重复已经产生了三个用户可见缺陷 —— 其中「安全开关假装生效」是严重级别。
2. **7 个环里 6 个是「搬函数 + 删一个死字段」就能解决的，测试改动合计 4 行。**
   不要把它想成需要重新设计的难题。
3. **`Config::client_auth_enabled` 是死字段，删掉即消环。** 纯减法。
4. **`stats` 不该拆，`settings`/`util`/`workbuddy_auth` 该拆，但病灶各不相同。**
   判据是「几件事共用一个名字」，不是行数。
5. **`oauth` 在库内零消费方**，是 `workbuddy_auth` 最容易切的一刀。
6. **不做的三件事要写进文档**，否则下个版本又会被重新讨论：
   `proxy.rs` 继续拆、`translate` 纯化、REST 面补测试（取决于决策 A）。
