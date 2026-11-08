# proxy-rs v3.5.3 发布说明：修掉「保存配置不生效」与「版本号对不上」

> 版本：**3.5.3**（上一版 3.5.2）
> 两个缺陷都是上一版发出去之后才被发现的，且都属于「测了，但测的不是真正会坏的那一环」。

---

## 1. 保存配置后，运行中的代理不重建（3.5.2 的分类器开关因此形同虚设）

### 现象

设置页打开「拦截 auto 模式的安全分类器」，点「保存配置」——提示`✓ 配置保存成功`，
`gui-settings.json` 里确实是 `intercept_safety_classifier: true`，但分类器请求**照旧转发上游**。
开关看起来无效。

### 根因

`Config` 是**快照**：`Config::from_settings` 只在监听器构建时跑一次。上游 URL、`model_map`、
`sanitize_terms`、`force_stream`、`intercept_safety_classifier` 全在那个快照里，路由一旦建好就
不再读设置文件。所以「保存配置」必须**重建监听器**才算生效。

3.4.0 之前，桌面端有自己的一份 `save_settings` 命令，结尾就是 `start_proxy_server(...)`。
3.4.0 A4 把桌面 59 个命令删到 5 个（净删 1954 行），这份 `save_settings` 作为「admin_rpc 已有
等价实现」被删掉了——**但 admin_rpc 那份只写文件**。于是"保存后重启代理"这一步随命令一起消失，
没有任何测试发现，因为剩下的测试断言的都是**文件**：

| 断言对象 | 3.5.2 时状态 |
|---|---|
| 文件里出现新值 | ✅ 有测试 |
| 入站鉴权开关热生效 | ✅ 有测试（3.0.2 修过一个同类的） |
| **运行中的代理采用新 Config** | ❌ **从来没有** |

### 修复

新增 `restart_service` 桌面钩子，两条保存路径（RPC 与 REST）都会调用它；桌面端实现
`restart_proxy_server`：取消旧监听器 → **等端口真正释放**（`bound_port` 归零）→ 按磁盘上的新
配置重新绑定。

三个细节是必须的，不是谨慎：

1. **等端口释放**。`axum::serve` 收到 cancel 后开始优雅关闭，但 socket 要等任务返回才关闭。
   释放前就重绑会撞上「address in use」——那个端口是我们自己占的——结果是服务停着不动，比不重启更糟。
2. **重启前重新读盘**。`run_proxy_server` 原先取的是 `ctx.settings` 缓存（进程启动时的快照），
   现在改为 `GuiSettings::load()` 后写回缓存。为了拿到刚保存的值而重启，却读旧缓存，
   等于原地打转。
3. **停止状态不偷偷启动**。此前保存设置会顺带拉起服务；修复后按原状态处理：本来停着就保持停着。

顺带修正 `start_proxy_server` 的一个隐患：它只看 `service_ctrl.is_running()`。而旧监听器退出时
会把状态标成 stopped、新实例启动前状态可能是「running 但无 socket」——现在要求
`running && bound_port != 0` 才走 resume，否则真的去绑定。

### 这个缺陷是怎么被证实的

不是看代码推断的。起真实网关、发一个形状与 Claude Code auto 模式分类器一致的请求
（1 条 message + 无 tools + 14 万字符 system），开关**关闭**时得到上游 400；通过管理接口把开关
**打开**、确认文件里已是 `true`、RPC 返回成功——**同一个进程再发一次，仍然是上游 400**，
且日志里没有任何 `intercepted` 行。修复后由新增测试守住这条链路。

### headless 部署的语义没有变

服务器模式下服务由 systemd 托管，网关不会自己重启，如实告知：
`设置已保存（服务器模式下服务由 systemd 托管，请用 systemctl restart proxy-rs）`。
这条也有测试——保存**必须成功**（文件确实写了，`systemctl restart` 之后就会生效），
把它报成失败会让运维去找一个不存在的问题。

---

## 2. 版本号三处不同源：装了 3.5.2，控制台显示 v3.5.1

### 现象

v3.5.2 的 dmg 实测：`Info.plist` 是 `3.5.2`（安装包、文件名都对），
但**控制台头部的版本号显示 v3.5.1**。

### 根因

`src-tauri/Cargo.toml` 的 `version` 停在 **3.5.1**，而 3.5.2 的 bump 提交
（`bcbce36`）只改了 4 个文件——`Cargo.lock` / `Cargo.toml` / `package.json` /
`tauri.conf.json`，**恰好漏掉 `src-tauri/Cargo.toml`**。此前 4 次 `chore(release)`
提交都是标准的「5 文件 6 行」，这一次少了一个。

两个不同的版本号各有各的消费者：

| 来源 | 决定什么 |
|---|---|
| `src-tauri/tauri.conf.json` | 安装包版本：`Info.plist`、MSI/NSIS、文件名 |
| `src-tauri/Cargo.toml` | `proxy-rs-gui` crate 的 `CARGO_PKG_VERSION`，即**控制台头部显示的版本** |

### 为什么 CI 结构上看不见

`cargo metadata` 显示 `workspace_default_members` 只有根包 `proxy-rs`。
`ci.yml` 里是 `cargo clippy --all-targets` 与 `cargo test`，**都跑不到 `proxy-rs-gui`**。
那三个 `env!("CARGO_PKG_VERSION")`（`tray.rs` 的 `desktop_status`、`main.rs` 的
`get_bootstrap_info`、`app/server.rs` 的 `AdminState`）在 CI 里从未被编译过。

而且 Linux CI 也**不能**简单地改成 `--workspace` 就完事：GUI crate 需要 webkit2gtk 等
Tauri 系统依赖，Linux runner 上装不齐，加了只会让 CI 变红。

### 修复

- 八处版本声明统一到 `3.5.3`（含此前一直停在 `3.4.1` 的 `package-lock.json` 两处）。
- 新增 `tests/release_consistency.rs`（**文本级**断言，不编译 GUI crate，所以 Linux CI 能跑）：
  - 所有声明点必须同号；
  - 单独断言事故那一对：`tauri.conf.json` = `src-tauri/Cargo.toml`；
  - 断言 `tray.rs` / `main.rs` / `app/server.rs` 仍从 `CARGO_PKG_VERSION` 取版本——
    将来有人换版本来源时会被提醒，因为上面的相等性正是它正确的前提。
- `ci.yml` 增加具名步骤（`cargo metadata --locked` + 该测试），**蹦进漏改文件时是一条点名的红**。
- `release.yml` 两道闸：触发从 `tags: ["*"]` 收窄为 `tags: ["v*"]`（原先任意 tag 都会发一个
  带安装包的公开 Release）；`Create release` 之前校验 **tag 名 = 安装包版本 = GUI crate 版本**。

---

## 3. 顺带修掉的测试隔离问题

都属于「在本机能过、在别人机器上不一定」的那一类，起因是测试读了不该读的进程全局状态。

- **`.env` 里的 `PROXY_API_TOKEN` 会击穿「未配置 token」的测试**。`dotenv_values()` 是个
  `OnceLock` 缓存，候选表第一项是 CWD 的 `.env`、第二项是 `~/.proxy-rs/.env`；测试只
  `remove_var` 进程变量，管不到文件。于是「未配置 token」的用例会用上一个它从未提及的
  token 去认证，得到 401 而不是期望的 503/403 —— 读起来像鉴权逻辑的 bug。

  复现条件要说准确：**必须是 CWD 的 `.env` 里含 `PROXY_API_TOKEN`**。候选表是
  `find_map`，CWD 的 `.env` 一旦存在（哪怕不含 token）就会命中并返回，**不再读**
  `~/.proxy-rs/.env`。所以单独在 HOME 放 `.env` 时旧代码反而是绿的；这也解释了它平时
  为什么不发作。

  修法：新增 `set_dotenv_override_for_tests`，测试用它把查找固定到「不读任何 `.env`」；
  `.env` 的候选路径同时改为经 `dotenv_path()`（= `data_dir()/.env`），于是设了
  `PROXY_DATA_DIR` 的测试会自动被重定向；lib 单测另由 `cfg!(test)` 直接返回空表。
  **实测**：CWD 的 `.env` 写入 token 后，修复前 `admin_api` 2 failed（`left: 401
  right: 200/503`）、`api_scheduler` 2 failed（`left: 401 right: 403`），修复后分别为
  **53 passed / 8 passed**。
- **集成测试写进开发者真实的 `~/.proxy-rs`**。`body_buffer_limit` 等没有重定向数据目录；
  因为集成测试是独立 crate，`src/settings.rs` 里 `cfg!(test)` 的 `/tmp` 分支不生效，
  于是日志落进真实 `~/.proxy-rs/logs/proxy.log`。现已统一补上 `PROXY_DATA_DIR` 与日志重定向。
  **实测**：修复后跑该测试，真实日志里那条 `exceeds the 4 KiB limit` 的时间戳不再前进
  （修复前每次运行都会新增，审计记录为 305→306 行）。
- 清理了 `v3.5.0` 时代修 HOME/DATA_DIR 双锁 bug 时留下的一个空目录残骸（未被 git 跟踪）。

---

## 验证

- `cargo test`：**786 passed / 0 failed**（新增 `release_consistency` 4 个 + `settings_hot_apply` 2 个）
- **反向验证过**：撤掉 `save_settings` 里的重启调用 → `settings_hot_apply` 两个测试立刻报红，
  失败信息直指「值到了文件、没到代理」；把 `src-tauri/Cargo.toml` 改回 3.5.2、把
  `package-lock.json` 改回 3.4.1 → `release_consistency` 分别报红并点名条目
- `cargo fmt --check` / `cargo clippy --all-targets -- -D warnings` 干净
- `check-ui` / `check-dist` / `build:ui` / `smoke` 全过
- headless 实测：保存后用 `systemctl restart` 语义提示，不再谎称已生效

无配置格式变更。

---

## 一个仍未修的已知问题（不在本版范围）

`data_dir()` 在 lib 单测中被触达时仍会在开发者真实 `~/.proxy-rs` 上 `create_dir_all`，
并可能读到真实的 `api-keys.json`。本版**已把集成测试二进制清成零残留**，剩下的全部集中在
lib 单测，已用假 HOME 逐模块归因：

| 模块 | 在假 HOME 下会创建 |
|---|---|
| `settings::` | `.proxy-rs-test-settings/`、`.proxy-rs/` |
| `proxy::` | `.proxy-rs/` |
| `scheduler_state::` | `.proxy-rs/scheduler-state.json` |
| `launch_agent::` | `.proxy-rs/logs/` |
| `config::` | `.proxy-rs/` |

机制已用 atime 变化证实（读到了真实的密钥池文件），但 `--test-threads=16` 的全量矩阵
20 轮未能自然复现出**失败**——它污染环境，不必然失败，所以优先级低于本版两个真实缺陷。
彻底的修法是给 `data_dir()` 一个测试专用 override（与 `set_log_file_override_for_tests`、
`set_dotenv_override_for_tests` 同构），影响面比这几处隔离修复大，留待单独一版处理，
避免把发布版塞进大改。
