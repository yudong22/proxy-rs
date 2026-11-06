# proxy-rs v3.2.1 发布说明：修复 Linux 发布物无法执行

> 版本：**3.2.1**（上一版 3.2.0）
> **v3.2.0 的 Linux 产物在多数服务器上根本起不来。** 本版修复它，并补上登录页的一个
> 关键属性、把服务器套件打成单一归档。

---

## 1. 严重：Linux 产物在本该支持的机器上无法启动

### 现象

v3.2.0 的 `proxy-rs-server-linux-x64` 在 Alibaba Cloud Linux 3（glibc 2.32）上：

```
$ ./proxy-rs-server-linux-x64
./proxy-rs-server-linux-x64: /lib64/libc.so.6: version `GLIBC_2.33' not found
./proxy-rs-server-linux-x64: /lib64/libc.so.6: version `GLIBC_2.34' not found
```

同一台机器上，我们**在本地编译**的同版本二进制工作正常——所以这不是功能问题，是
**构建方式**问题。

### 根因

产物用 `x86_64-unknown-linux-gnu`（动态链接 glibc），而**构建机决定了运行机的 glibc 下限**。CI 在 `ubuntu-22.04` 上编译，它的 glibc 是 2.35，其中 libpthread 已被合并进 libc，
Rust 标准库因此绑定了 2.34 版的 `pthread_*` 符号。

`objdump -T` 直接看到：

```
GLIBC_2.34  __libc_start_main
GLIBC_2.34  pthread_create
GLIBC_2.33  fstat64
```

后果：Ubuntu 20.04（2.31）、Debian 11（2.31）、RHEL 8（2.28）、Alibaba Cloud Linux 3（2.32）
**全都起不来**。

### 为什么不能"换个老 runner"

`ubuntu-20.04` 已于 **2025 年 4 月彻底退役**。钉在这个 label 上会让发布流程永远等不到
runner——正是当初 `macos-13` 那个坑的重演：一个跑不起来的矩阵分支会把整个 release 卡死。

### 修复：静态链接 musl

改用 `x86_64-unknown-linux-musl`，**完全不依赖目标机的 glibc 版本**，顺带支持 Alpine。

代价：musl 用 mallocng 分配器，在多线程密集分配时略慢。对这个以网络 I/O 为主的代理
没有可测差异，而可移植性更重要。

### 让发布流程自己证明"能跑"

原本的流程只验证"编译成功 + 上传成功"——v3.2.0 正是这样通过的。现在加了两道**运行时**验证：

| 步骤 | 做什么 |
|---|---|
| **老 glibc 容器验证** | 在 `debian:11`（glibc **2.31**，比目标机还旧）里启动二进制，等 `/health` 返回 200，再确认 `/admin/login` 也是 200 |
| 构建机直接运行 | 同样的探活，覆盖 musl 运行时的基本情况 |

用老容器而不是构建机，是关键：在 glibc 2.35 上跑一个要求 2.34 的二进制**会通过**，
而这正是当初漏掉的原因。debian:11 比目标机更旧，通过才算真证据。

一旦产物是动态链接的，`Package` 步骤直接失败：

```bash
if file dist/... | grep -q "dynamically linked"; then
  echo "::error::the Linux artifact is dynamically linked; it must be static musl"
  exit 1
fi
```

---

## 2. 登录页：`readonly` 让浏览器不提示保存密码

上一版加了一对 username/password 字段，但 username 带了 `readonly`。这仍然不够：

| 写法 | 结果 |
|---|---|
| 没有 username 字段 | 没有可配对的凭据 |
| `hidden` / `display:none` | 字段被跳过 |
| **`readonly`** | **Chrome 判定为"页面自己造的假值"，整个表单不再被识别为登录表单** |

三者在实测中都各自致命。现在 username 是**可见、可编辑**的字段，值 `admin` 只是密码管理器
存的标签——服务端完全忽略它，只用 token 鉴权，所以随便改也不影响登录。

同时保留上一版的改动：原生表单 POST（浏览器只认真实导航，`fetch` 登录不触发保存提示）。

---

## 3. Linux 发布物改为单一归档

上一版在 Release 页面上摊了 4 个文件（二进制、sha256、systemd 单元、nginx 配置），
下载和对照都零碎。现在是一个包：

```
proxy-rs-server-linux-x64.tar.gz
proxy-rs-server-linux-x64.tar.gz.sha256
└── proxy-rs-server-linux-x64/
    ├── proxy-rs-server           静态链接，无运行时依赖
    ├── proxy-rs.service          systemd 单元
    ├── nginx-site.conf.example   反代示例（域名要改）
    └── DEPLOY.md                 部署文档
```

把 nginx 配置改名成 `.example` 还解决了一个问题：原来的文件名里带着我们的真实域名，
放进公共发布页并不合适。

---

## 4. 验收

| 项 | 预期 | 实测 |
|---|---|---|
| `cargo test` | 全绿 | ✅ 703 passed |
| `cargo clippy -D warnings` | 无告警 | ✅ |
| `cargo fmt --check` | 无差异 | ✅ |
| `release.yml` YAML | 可解析、9 个步骤 | ✅ |
| 归档结构 | 4 个文件在单一 tar.gz 内 | ✅ 本地模拟验证 |
| 生产登录页 username | **非 readonly**、可见、autocomplete 配对 | ✅ 浏览器实测 |
| 生产管理台全流程 | `admin-smoke` 全通过 | ✅ |
| 生产服务 | active / enabled | ✅ |

> **静态 musl 产物的实际运行**由 CI 在 `debian:11` 容器里验证，本地无法交叉编译复现
> （Docker Hub 不可达）。这是本版唯一依赖 CI 而非本地实测的项，发布页上的产物以 CI
> 那次运行为准。

---

## 5. 升级

```bash
# 下载新归档
curl -LO https://github.com/yudong22/proxy-rs/releases/download/v3.2.1/proxy-rs-server-linux-x64.tar.gz
sha256sum -c proxy-rs-server-linux-x64.tar.gz.sha256
tar xzf proxy-rs-server-linux-x64.tar.gz
cd proxy-rs-server-linux-x64

systemctl stop proxy-rs
install -m 755 proxy-rs-server /opt/proxy-rs/bin/proxy-rs-server
systemctl start proxy-rs
curl -s http://127.0.0.1:3456/health
```

**如果你在 v3.2.0 时是照文档自己编译的**，那份产物一直是好的（本机 glibc 匹配），可以不换。

**反代配置**：`X-Forwarded-For` 必须用 `$remote_addr` 覆盖（不能是
`$proxy_add_x_forwarded_for`），否则登录限速仍可被伪造地址绕过。见
`nginx-site.conf.example`。
