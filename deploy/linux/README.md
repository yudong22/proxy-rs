# Linux 服务器部署（headless 网关）

> 目标：把 Proxy RS 网关部署到一台 Linux 服务器（无图形界面），通过 nginx 反代对外提供 `/v1/*`。
> 本文内容来自 2026-11-06 在阿里云 Alibaba Cloud Linux 3（`47.80.20.105`，域名 `api.wuba.site`）上的实际部署与验收，不是推测。

---

## 最终形态

```
客户端 ──HTTPS──► nginx:443 ──HTTP──► 网关 127.0.0.1:3456 ──► 上游
                    ↑ 证书/路径边界/SSE         ↑ systemd 托管
```

| 组件 | 形态 | 托管方式 |
|---|---|---|
| 网关 | `proxy-rs-server`（6.7 MB 单文件，无 Tauri / 无 GUI 依赖） | systemd `proxy-rs.service`（enabled，开机自启） |
| 反代 | 系统 nginx | 已有 nginx，新增 `api.wuba.site` vhost |
| 证书 | Let's Encrypt（acme.sh，webroot 模式） | acme.sh cron，自动续期并 `systemctl reload nginx` |

### 为什么是 headless 二进制

桌面版 `proxy-rs-gui` 需要图形会话、窗口和托盘图标。服务器上这些都不存在，而且桌面版会随登录会话结束而退出。`src/router.rs` 的模块注释本来就写明它「kept free of GUI types so a headless front-end can reuse it unchanged」，`src/bin/proxy-rs-server.rs` 就是那个 front-end。

它做的事只有：读配置 → `build_app_router` → `start three scheduler loops` → `axum::serve`。**没有**热重载、托盘、启停控制或 GUI 命令面 —— 服务器的配置接口就是「改文件 + 重启」，systemd 已经提供了这个接口。

---

## 一、构建

服务器有 Rust 工具链时**直接在服务器上编译**最省事（本机上也一样，产物与目标机 glibc 天然对齐）：

```bash
# 一次性：装工具链（约 350 MB 磁盘）
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal

cd /path/to/proxy-rs
export PATH="$HOME/.cargo/bin:$PATH"
cargo build --release --bin proxy-rs-server
```

> 实测：2 核 / 1.9 GB 内存的机器上 release 构建约 **3 分 40 秒**。构建由 `panic = "abort"` + LTO 产出 6.7 MB 单文件。

没有工具链的机器上交叉编译也可行，但要注意 glibc 版本：产物链接目标机的 glibc，**在更新的机器上编译、往更旧的机器上推会缺符号**。用容器或与目标同版本的构建机是可靠做法。

---

## 二、安装

```bash
# 目录约定
/opt/proxy-rs/bin/      proxy-rs-server
/opt/proxy-rs/data/     数据目录（PROXY_DATA_DIR）：凭据、settings、stats.db、logs
/etc/proxy-rs.env       环境配置 + 密钥（0600，root:proxyrs）

# 1) 专用非特权用户
useradd --system --home-dir /opt/proxy-rs --shell /sbin/nologin proxyrs
install -d -o proxyrs -g proxyrs /opt/proxy-rs/{bin,data,logs}
install -m 755 target/release/proxy-rs-server /opt/proxy-rs/bin/

# 2) 配置（见下方 .env 示例）
install -m 600 -o root -g proxyrs ./proxy-rs.env /etc/proxy-rs.env

# 3) systemd
install -m 644 deploy/linux/proxy-rs.service /etc/systemd/system/proxy-rs.service
systemctl daemon-reload
systemctl enable --now proxy-rs
systemctl status proxy-rs --no-pager
```

### `/etc/proxy-rs.env` 示例

```ini
UPSTREAM_BASE_URL=https://copilot.tencent.com/v2/chat/completions

# 只绑回环：nginx 是唯一客户端，网关不直接对外
PROXY_BIND=127.0.0.1
PORT=3456
PROXY_DATA_DIR=/opt/proxy-rs/data

# 必须开启。关掉等于「能访问到 nginx 的人都能花你的上游额度」
PROXY_CLIENT_AUTH=true

# /api/* 写路由的令牌。不配则 POST /api/scheduler/run/* 与 /api/admin/* 一律 403
PROXY_API_TOKEN=<openssl rand -base64 32>
```

> `.env` 里这条 **`PROXY_CLIENT_AUTH=true` 是本部署与桌面版最大的语义差异**。桌面版默认关闭（升级后突然要 key 会让所有客户端挂掉），服务器默认必须显式打开。网关启动日志会明确打印 `client_auth=true/false`，关闭时还会有一条 WARN —— 这是唯一能确认「到底开没开」的地方。

### 数据目录从桌面端迁移

把桌面机的 `~/.proxy-rs/` 下这几个文件复制到 `/opt/proxy-rs/data/`：

| 文件 | 作用 |
|---|---|
| `gui-settings.json` | 厂商、端口、模型映射（**需把 `bind` 改成 `127.0.0.1`、`client_auth_enabled` 改 `true`**） |
| `workbuddy-credentials.json` | WorkBuddy 登录态凭据池 |
| `api-keys.json` | 上游 API 密钥池 |
| `workbuddy-pool.json` | 默认身份与打卡偏好 |
| `client-accounts.json` | **入站**访问账户（只存 SHA-256，不存明文） |
| `scheduler-state.json` | 三个循环的「上次运行」记录（可搬可不搬，纯观测） |

**不要搬 `stats.db`**：那是这台机器自己的统计库，换机会把两台的计数混在一起。

复制后收紧权限：`chmod 700 /opt/proxy-rs/data && chmod 600 /opt/proxy-rs/data/*.json`。

> **访问账户的明文 key 搬不过来**。`client-accounts.json` 只存哈希，所以从桌面端迁移后**必须重新签发一把 key**，见 §5。

---

## 三、nginx 反代

配置见本目录 [`api.wuba.site.conf`](api.wuba.site.conf)。宝塔面板管的机器放到 `/www/server/panel/vhost/nginx/` 下；原生 nginx 放到 `conf.d/` 并自行 `include`。

### 暴露边界（这是配置里最重要的部分）

| 路径 | 处理 | 理由 |
|---|---|---|
| `/v1/*`、`/responses`、`/chat/completions`、`/backend-api/*` | 反代到网关 | 数据面 |
| `/admin/` | 反代到网关的 `/admin/api/` | **Web 管理台**（自带令牌登录） |
| `/` | 静态首页 | 使用说明，不含凭据 |
| `/api/*`、`/metrics`、`/health` | **`return 404`** | 内部只读观测命名空间 |

> **`/admin/` 与 `/api/` 是两个不同的命名空间，别混。** `/api/*` 是**无鉴权**的只读观测面（本机 dashboard 用），所以必须在反代层挡掉；`/admin/api/*` 是**有鉴权**的管理面（令牌换 Cookie 会话），可以对外。
>
> nginx 把 `/admin/*` 重写成 `/admin/api/*`（`rewrite ^/admin/?(.*)$ /admin/api/$1 break`），这样对外是干净的 `/admin` 前缀，而网关只认一个绝对前缀，少一处必须同步的常量。

`/api/*` 里有一条**会改上游账号状态**的 `POST /api/scheduler/run/:loop`。网关侧已有 `PROXY_API_TOKEN` 校验，但那是第二道锁 —— 在反代层把整个命名空间挡掉，外部根本看不到这把锁存在。同理 `/metrics` 与 `/health` 只对回环有意义，没有任何理由出现在公网。

### SSE 必须关缓冲

```nginx
proxy_buffering off;
```

这一行是整套配置里最关键的。nginx 默认 `proxy_buffering on`，会把上游增量攒起来，客户端要等整轮生成结束才收到第一帧 —— 表现为「号称支持流式，实际不流」。

### 超时

网关侧上游读超时是 300 s（`reqwest::read_timeout`）。反代超时必须**不短于**它，否则 nginx 会先掐断一条网关仍在等上游的长回复。本配置给 600 s。

### 请求体上限

`client_max_body_size 40m`。网关自己的 `PROXY_MAX_BODY_BYTES` 默认 32 MiB（长会话用）。反代这一层必须**不小于**它，否则大请求在到达网关前就被 413 掉，而**网关日志里什么都不会留下** —— 排查时最容易被误判成客户端的问题。

---

## 四、证书

用 acme.sh 的 webroot 模式，与网关无关：

```bash
curl https://get.acme.sh | sh -s email=admin@example.com

# 前提：nginx 已放行/.well-known/acme-challenge/（配置里已有）
~/.acme.sh/acme.sh --issue -d api.wuba.site \
  --webroot /www/wwwroot/api.wuba.site --server letsencrypt

# 装到 nginx 读的路径，并把 reload 挂上去（续期后自动生效）
~/.acme.sh/acme.sh --install-cert -d api.wuba.site --ecc \
  --key-file       /www/server/panel/vhost/cert/api.wuba.site/privkey.pem \
  --fullchain-file /www/server/panel/vhost/cert/api.wuba.site/fullchain.pem \
  --reloadcmd      "systemctl reload nginx"
```

> **先加 DNS 记录再加 vhost**。DNS 没生效时 `nginx -t` 会因为证书文件不存在直接失败（`BIO_new_file() failed`）。想先验证配置语法，可以先放一份自签证书占位，签发后再 `--install-cert` 覆盖。
>
> acme.sh 装完会自己写 cron，续期无需额外操作。实测下次续期由 ARI 窗口决定（本例为 2027-01-05）。

---

## 五、签发访问密钥

`client-accounts.json` 只存哈希，所以从别处搬过来的池子**没有可用 key**。用管理接口现签一把（走回环，需 `PROXY_API_TOKEN`）：

```bash
TOKEN=$(grep '^PROXY_API_TOKEN=' /etc/proxy-rs.env | cut -d= -f2)

curl -s -X POST http://127.0.0.1:3456/api/admin/client-accounts \
  -H "Authorization: Bearer $TOKEN" \
  -H 'Content-Type: application/json' \
  -d '{"label":"server-prod","note":"krlinux gateway"}'
```

返回体里的 `api_key`（形如 `prs_xxxxxxxx...`）**只出现这一次**，请立即交给客户端使用：

```json
{"account":{...},"api_key":"prs_...","header":"x-proxy-rs-key"}
```

三种请求头任选其一，效果相同：`x-proxy-rs-key`、`Authorization: Bearer`、`x-api-key`。

> 管理接口**只在回环上**可用 —— 这正是 §3 把 `/api/*` 在 nginx 层挡掉的原因。要从外部管理，走 SSH 隧道：`ssh -L 3456:127.0.0.1:3456 krlinux`。

---

## 六、验收清单

下表的「实测」列是 2026-11-06 在 `api.wuba.site` 上的真实结果。

| 项 | 期望 | 实测 |
|---|---|---|
| `systemctl is-active proxy-rs` | `active` | ✅ |
| `systemctl is-enabled proxy-rs` | `enabled`（开机自启） | ✅ |
| `GET /health`（回环） | 200 `OK` | ✅ |
| `GET /` 公网 | 200 首页 | ✅ |
| 无 key `GET /v1/models` | 401 | ✅ |
| 带 key `GET /v1/models` | 200 模型列表 | ✅ |
| 错误 key | 401 | ✅ |
| 三种请求头 | 均 200 | ✅ 全部通过 |
| 带 key `POST /v1/messages` | 200 真实转发 | ✅ `hy4`，usage 正常 |
| SSE 流式 | 逐条到达，不缓冲 | ✅ `content_block_delta` 分多帧 |
| `/api/*`、`/metrics`、`/health` 公网 | 404 | ✅ 全部被 nginx 挡 |
| 未知路径 | 404 | ✅ |
| TLS 证书 | Let's Encrypt 有效链 | ✅ `CN=api.wuba.site`，issuer `YE1` |
| `systemctl restart` | 自动恢复 | ✅ |
| `kill -9` 主进程 | systemd 自动拉起 | ✅ 6 s 内恢复 |
| 数据目录权限 | 0700，文件 0600 | ✅ |
| `x-forwarded-for` / `X-Real-IP` | 真实客户端 IP | ✅ 网关日志可见 |

---

## 七、踩过的坑

| 坑 | 症状 | 正解 |
|---|---|---|
| 先建 vhost、后加 DNS | `nginx -t` 报 `BIO_new_file() failed ... fullchain.pem` | 证书文件由签发步骤产生；先用自签占位，签发后覆盖 |
| 转发时只看到 `message_start`/`stop` | 以为 nginx 吞了 `content_block_delta` | 实际是 `\| head -N` 的管道缓冲在提前关管道。用 `cat -A` 看原始字节，delta 是完整的 |
| `bind` 沿用了桌面端配置 | 网关试图绑 `0.0.0.0` 或旧端口 | 迁移 `gui-settings.json` 后必须改 `bind` / `port` |
| 从别处搬 `client-accounts.json` 就直接用旧 key | 一直 401，且没有明文可查 | 只存哈希，必须重新签发（§5） |
| 忘设 `PROXY_CLIENT_AUTH` | 数据面完全不鉴权 | 启动日志有 WARN；`.env` 里显式 `=true` |
| 反代 `client_max_body_size` 太小 | 大请求 413，网关日志空白 | 反代上限 ≥ 网关 `PROXY_MAX_BODY_BYTES` |
| 反代不关 `proxy_buffering` | 流式响应攒成一整块 | `proxy_buffering off` |

### 排障顺序

```bash
systemctl status proxy-rs --no-pager      # 起没起来
journalctl -u proxy-rs -n 50 --no-pager   # 启动日志里的 bind / client_auth
ss -ltnp | grep 3456                      # 端口在不在、绑在哪
curl -s http://127.0.0.1:3456/health      # 绕过 nginx 打网关本体
curl -sI https://api.wuba.site/           # 再看反代层
```

**先分清是哪一层**：回环通、公网不通是 nginx 的问题；回环都不通是网关的问题。跳过一次只会浪费一整轮。

---

## 八、与桌面端同时运行（重要）

同一套 WorkBuddy 登录态被两个进程同时维护时，**刷新令牌会互相消费**：A 刷了新 token，B 手里那个就失效了，最终账号被上游标成「需重新绑定」。

若两台同时跑，至少做到：

- **只让一台负责账号维护**。把另一台的每日打卡与成长任务关掉（`workbuddy-pool.json` 里的 `daily_checkin_enabled` / `growth_enabled`），只保留数据面转发。
- 或让两台**不共享** `workbuddy-credentials.json`：服务器用独立的 `api-keys.json`（静态上游密钥）转发，不参与登录态刷新。

已知取舍：即使关掉定时循环，登录态**巡检**（5 分钟一次，到期才续）与请求路径上的 401 反应式刷新仍会写 token。彻底避免只能二选一。

---

## 九、为何不用 Cloudflare Tunnel

Windows 那台用隧道是因为腾讯云对未备案域名做 Host 劫持。**阿里云这台没有这个问题**：`api.wuba.site` 解析到本机 IP，直连 443 实测 200，不需要隧道。少一个组件就少一个故障点。

若将来需要绕开备案或隐藏源站 IP，可以照搬 `deploy/win/README.md` 的隧道方案，把 cloudflared 指到本机 `80`。
