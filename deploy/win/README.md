# Windows 服务器部署（实测记录）

> 目标：把 Proxy RS 网关部署到 Windows Server，通过公网 HTTPS 对外提供 `/v1/*` 服务。
> 本文内容全部来自 2026-11-05 在腾讯云 Windows Server 2019（`101.35.234.6`，域名 `gate.wuba.site`）上的实际部署，不是推测。

---

## 最终形态

```
客户端 ──HTTPS(443)──► Caddy ──HTTP──► 网关(127.0.0.1:3456) ──► 上游
         ↑ Let's Encrypt        ↑ 只放行 /v1/* 与首页
```

| 组件 | 进程 | 会话 | 托管方式 |
|---|---|---|---|
| 网关 | `proxy-rs-gui.exe` | session 2 | 计划任务 `ProxyRS-Launch`（InteractiveToken）|
| 反代 | `caddy.exe` | session 0 | 计划任务 `ProxyRS-Caddy`（AtStartup + 失败重启）|

目录约定（**不要放 C 盘**，实测仅剩 6 GB）：

```
E:\proxy-rs\bin\         proxy-rs 安装器、caddy.exe
E:\proxy-rs\data\        （预留）
E:\proxy-rs\deploy\      Caddyfile、landing\index.html、安装日志
E:\proxy-rs\logs\        caddy 日志
%USERPROFILE%\.proxy-rs\ 网关数据（凭据、设置、stats.db、日志）
```

---

## 关键实测结论

### 1. RDP 断开**不会**杀死网关（已实测）

2026-11-05 20:47 实测：`quser` 显示会话 2 状态为「断开」，而：

```
caddy          pid=328   session=0   存活
proxy-rs-gui   pid=2140  session=2   存活
80 / 443 / 3456 均在监听
```

公网实测同样正常（首页 200、带 key 200、无 key 401、真实转发成功）。

**但这是有条件的**：Windows 默认会在 RDP 断开一段时间后**注销**会话（`MaxDisconnectionTime`），注销时 session 2 内的进程会被杀。当前能存活是因为断开时间还短、且该策略未被触发。

若要长期稳定，二选一：

- **改注册表**延长/禁用断连注销（`HKLM\SOFTWARE\Policies\Microsoft\Windows NT\Terminal Services` 下的 `MaxDisconnectionTime` / `MaxIdleTime` / `MaxConnectionTime` 设为 `0`）；
- **改用 headless 二进制**（推荐，见 §6）。

> 曾经的误判：早先 SSH 探测到 `mainWindowHandle=0`、窗口枚举为空，据此推断"窗口没创建"。实际上 SSH 落在 **session 0**，而窗口在 **session 2** 的桌面上，跨会话枚举本来就看不到。`msedgewebview2.exe` 有 6 个进程在跑，也印证 WebView2 引擎工作正常。

### 2. 数据目录的 `HOME` 陷阱（v3.0.2 已修）

Windows **不设置 `HOME`**。修复前 `data_dir()` 返回 `None`，各 store 兜底到 `current_dir()`，而 Explorer 启动的 GUI 其 CWD 正是 `C:\Program Files\Proxy RS\`，导致：

```
C:\Program Files\Proxy RS\workbuddy-credentials.json   13047 B
C:\Program Files\Proxy RS\client-accounts.json           553 B
```

后果是升级/卸载会带走凭据。v3.0.2 起 `home_dir()` 按 `HOME` → `USERPROFILE` → `HOMEDRIVE`+`HOMEPATH` 解析。实测升级后新写入只落 `%USERPROFILE%\.proxy-rs\`。

### 3. 从 3.0.x 升级需手工搬数据

```powershell
$src = "C:\Program Files\Proxy RS"
$dst = "$env:USERPROFILE\.proxy-rs"
New-Item -ItemType Directory -Force -Path $dst | Out-Null
foreach ($f in @("workbuddy-credentials.json","api-keys.json","client-accounts.json",
                 "workbuddy-pool.json","gui-settings.json")) {
  if (Test-Path "$src\$f") { Copy-Item "$src\$f" $dst -Force; "moved $f" }
}
```

### 4. Caddy 的三个坑

| 坑 | 症状 | 正解 |
|---|---|---|
| `email` 写在站点块内 | `unrecognized directive: email` | 必须放进**全局选项块** `{ email ... }`，且在所有站点块之前 |
| `timeouts` 子指令不存在 | `unrecognized timeouts subdirective 'read_body'` | Caddy 2.11 的 `http.handlers.timeouts` 没有这些子指令，直接不写（默认无写超时，正适合长回复/SSE） |
| 兜底 `respond 404` 无 matcher | **所有路径**（含 `/v1/models`）都 404，看起来像反代没配 | `respond` 按顺序求值，无 matcher 会吃掉一切；必须写成 `@unknown not path / /v1/* ...` |

### 5. `caddyserver.com/api/download` 下到的文件不能直接用

按该 URL 下载得到 15.7 MB 文件，`MZ` 头正确、`file` 认作 PE，但 `caddy version` 无输出、无法执行。改用 GitHub 官方 release 的 `caddy_2.11.9_windows_amd64.zip`（解压后 50 MB，带完整模块）才正常。

### 6. 长期托管建议：headless 二进制

GUI 版依赖交互桌面（session 2），受 §1 的断连注销限制，且升级需人工。`router::build_app_router` 的注释明确写了它「kept free of GUI types so a headless front-end can reuse it unchanged」，抽一个不含 Tauri 的 bin 即可：

- 产物零 GUI 依赖（无 WebView2）
- 可注册为 `S4U` / `LocalSystem` 计划任务，不依赖任何登录会话
- 开机自启、无人值守

---

## 部署步骤

### 准备

```powershell
New-Item -ItemType Directory -Force -Path E:\proxy-rs\bin,E:\proxy-rs\deploy,E:\proxy-rs\logs
```

### 安装网关

```powershell
msiexec /i E:\proxy-rs\bin\Proxy.RS_x.y.z_x64_en-US.msi /qn /norestart /l*v E:\proxy-rs\deploy\msi.log
```

安装后 TCP 3456 默认绑 `127.0.0.1`，**不要**改成 `0.0.0.0`（由 Caddy 接入）。

### 安装 Caddy

```powershell
# 从 GitHub release 取 zip（不要用 caddyserver.com/api/download）
# https://github.com/caddyserver/caddy/releases/download/v2.11.9/caddy_2.11.9_windows_amd64.zip
Expand-Archive caddy.zip -DestinationPath E:\proxy-rs\bin -Force
E:\proxy-rs\bin\caddy.exe version   # 应输出 v2.11.9
E:\proxy-rs\bin\caddy.exe validate --config E:\proxy-rs\deploy\Caddyfile
```

`Caddyfile` 与 `landing/index.html` 见本目录，推到 `E:\proxy-rs\deploy\`。

### 防火墙

```powershell
foreach ($port in @(80,443)) {
  New-NetFirewallRule -DisplayName "ProxyRS-HTTPS-$port" -Direction Inbound `
    -Action Allow -Protocol TCP -LocalPort $port
}
```

云厂商**安全组**是第二道，必须同步放行 80/443。只改一处会表现为「本机能连、外网超时」——超时（而非拒绝）通常就是安全组没放行。

### 计划任务

`ProxyRS-Caddy`（开机自启 + 失败重试）：

```xml
<Triggers><BootTrigger><Delay>PT20S</Delay></BootTrigger></Triggers>
<Principals><Principal id="Author"><UserId>S-1-5-18</UserId>
  <RunLevel>HighestAvailable</RunLevel></Principal></Principals>
<Settings>
  <ExecutionTimeLimit>PT0S</ExecutionTimeLimit>
  <RestartOnFailure><Interval>PT1M</Interval><Count>999</Count></RestartOnFailure>
</Settings>
<Actions><Exec><Command>E:\proxy-rs\bin\caddy.exe</Command>
  <Arguments>run --config "E:\proxy-rs\deploy\Caddyfile"</Arguments></Exec></Actions>
```

---

## 验收清单

| 项 | 期望 | 实测 |
|---|---|---|
| `GET /` | 200 HTML 使用说明 | ✅ |
| 无 key `GET /v1/models` | 401 `missing_key` | ✅ |
| 带 key `GET /v1/models` | 200 模型列表 | ✅ |
| 带 key `POST /v1/messages` | 200 真实转发 | ✅ |
| SSE 流式 | 逐条到达，不缓冲 | ✅ `message_start` +1s → `delta` 逐条 |
| `/api/*`、`/metrics`、`/health` | 404（反代挡） | ✅ |
| `http://` | 308 → HTTPS | ✅ |
| TLS 证书 | Let's Encrypt 自动签发 | ✅ 有效期至 2027-02-03 |
| RDP 断开后 | 服务仍可用 | ✅（有条件，见 §1）|
