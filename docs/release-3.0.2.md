# proxy-rs v3.0.2 发布说明：修复 Windows 数据目录错位与鉴权开关不热生效

> 版本：**3.0.2**（上一版 3.0.1）
> 两个在真实 Windows Server 部署中暴露的 bug。**都是数据安全级别的**，建议所有 Windows 用户升级。

---

## Bug 1：Windows 上凭据文件写进 `C:\Program Files`（严重）

### 现象

在真实 Windows Server 2019 上通过 MSI 安装、从桌面启动后，**全部状态文件落到了安装目录**：

```
C:\Program Files\Proxy RS\workbuddy-credentials.json   13047 B
C:\Program Files\Proxy RS\api-keys.json                  230 B
C:\Program Files\Proxy RS\client-accounts.json           543 B
C:\Program Files\Proxy RS\gui-settings.json              287 B
```

而 `%USERPROFILE%\.proxy-rs\` 里只有 `stats.db` 和日志。

### 根因

`data_dir()` 只读 `HOME`：

```rust
let home = std::env::var("HOME").ok()?;   // Windows 上返回 None
PathBuf::from(home).join(".proxy-rs")
```

**Windows 不设置 `HOME`** —— 那是 Unix 约定，Windows 用 `USERPROFILE`。于是 `data_dir()` 返回 `None`，各 store 的兜底是 `PathBuf::from(".")`，即**当前工作目录**。GUI 由 Explorer 启动时 CWD 正是安装目录。

### 两个后果

1. **升级/卸载会带走凭据** —— 数据在安装目录里，MSI 卸载直接删掉，账号池和访问 key 全丢；
2. **写入落在受保护目录** —— 能写成功是靠 UAC 文件虚拟化，这是静默的、不可依赖的行为。

### 修复

新增 `settings::home_dir()`，Windows 用户可知：`HOME` → `USERPROFILE` → `HOMEDRIVE`+`HOMEPATH`。并把**全部 11 处** `HOME` 直读统一改为走它：

| 文件 | 原用途 |
|---|---|
| `settings.rs::data_dir` | 数据目录（**本 bug 主因**） |
| `settings.rs::env_data_dir` | `PROXY_DATA_DIR=~/...` 展开 |
| `config.rs` | `.env` 候选路径（`~/.proxy-rs/.env` 之前**在 Windows 上从未被探测**）|
| `claude_config.rs` | `~/.claude/settings.json` |
| `codex_config.rs` | `~/.codex/config.toml` |
| `dsh_config.rs` | `~/.dsh` |
| `pool_transfer.rs` | 导出默认目录 `~/Downloads` |
| `launch_agent.rs` | macOS LaunchAgent（一致性） |

顺带修好一条隐藏问题：文档承诺的 `~/.proxy-rs/.env` 位置**在 Windows 上原本根本读不到**，所以 `PROXY_CLIENT_AUTH=true`、`PROXY_API_TOKEN` 这类写在 `.env` 里的配置全部失效。

---

## Bug 2：`client_auth_enabled` 改了不生效，必须重启整个应用

### 现象

GUI 里勾选「启用入站鉴权」→ 保存 → 设置文件确实写了 `client_auth_enabled: true` → **但请求仍然不带 key 就能通过**。只有退出应用再启动才生效。

### 根因

`ClientAuthState` 在进程启动时构造一次，之后**只有代理监听器会随设置保存重启，它不会**。而 GUI 保存时的热重启只重建 router，读取的是旧的 `ClientAuthState`。

### 修复

`save_settings` 在保存成功后、重启代理前，直接更新活状态：

```rust
let auth_enabled = client_accounts::enabled_from(body.client_auth_enabled);
ctx.client_auth.set_enabled(auth_enabled);
ctx.client_auth.reload_from_store().await;
```

`reload_from_store()` 同时保证在别处（GUI 另一处 / HTTP 管理接口）新增的账户也被立即拾取。

---

## 测试

新增 `settings::tests` 4 项，直接钉住 Windows 行为：

| 用例 | 断言 |
|---|---|
| `home_dir_prefers_home` | `HOME` 优��于 `USERPROFILE`（Unix 不回归） |
| `home_dir_falls_back_to_userprofile_when_home_is_unset` | **`HOME` 缺失时用 `USERPROFILE`**（本 bug 的回归测试） |
| `home_dir_accepts_the_legacy_windows_pair` | `HOMEDRIVE`+`HOMEPATH` 正确拼合 |
| `home_dir_rejects_empty_values` | 空值不算 home（否则又会退回相对路径） |

全量：`cargo test` **650 项全绿**；`clippy -D warnings`、`fmt` 干净。

---

## 升级后需要做一件事

如果你已在 Windows 上部署过 3.0.x，**旧数据在安装目录里**。升级后会按新规则到 `%USERPROFILE%\.proxy-rs\` 找数据，那里是空的 —— 需要手工搬一次：

```powershell
$src = "C:\Program Files\Proxy RS"
$dst = "$env:USERPROFILE\.proxy-rs"
New-Item -ItemType Directory -Force -Path $dst | Out-Null
foreach ($f in @("workbuddy-credentials.json","api-keys.json","client-accounts.json",
                 "workbuddy-pool.json","gui-settings.json")) {
  if (Test-Path "$src\$f") { Copy-Item "$src\$f" $dst -Force; "moved $f" }
}
```

搬完重启应用。之后数据固定在用户目录，升级和卸载都不会再动它。

---

## 兼容性

- 无配置格式变更，`gui-settings.json` 与导出包格式不变（仍是 bundle v2）。
- macOS / Linux 行为不变：`HOME` 存在时仍是第一优先级。
- 若已用 `PROXY_DATA_DIR` 显式指定目录，该变量优先级最高，不受影响。
