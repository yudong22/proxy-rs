# proxy-rs v3.5.2 发布说明：Claude Code 权限模式一键写入 + 分类器开关

> 版本：**3.5.2**（上一版 3.5.1）
> 两处新能力，都来自对 Claude Code 2.1.311 的实测。默认行为不变。

---

## 1. 一键写入 Claude Code 的权限模式

### 背景

Claude Code 在「要不要执行这个工具调用」上有三条路，代价完全不同：

| `permissions.defaultMode` | 行为 | 每次工具调用的额外请求 |
|---|---|---|
| `default` / `manual` | 每次都询问 | 无（但非交互场景直接拒绝） |
| `auto` | 分类器逐条判定 | **有**，见下方实测 |
| `bypassPermissions` | 全部放行 | **无** |

本机实测（连续 3 次工具调用，Claude Code 2.1.311，经本网关计数）：

| 模式 | 上游请求数 |
|---|---|
| `bypassPermissions` | **4** |
| `auto` | **7**（多出 3 次判定，输出仅 4 token，输入可达 3 万+） |

### 改动

设置页「Claude Code 一键写入」新增**权限模式**下拉，写入 `~/.claude/settings.json`：

```
permissions.defaultMode          = 所选模式
skipDangerousModePermissionPrompt = true   （仅 bypassPermissions）
```

第二个键**不是可选项**。`bypassPermissions` 在 CLI 侧被一个**同意记录**门控着：
缺了它，CLI 首次进入该模式要弹窗确认，而后台 / `--bg` 会话会被**静默降级回
`default`**——表现为「点了没生效、还是在问」。手动接受那个弹窗写的就是同一个键，
所以一键写入是替用户记录这份同意，不是绕过它。

两个实测确认的坑，已在代码注释与文档里写明：

1. **作用域**：`auto` 与 `bypassPermissions` 只在**用户设置**下生效；自 v2.1.257 起写在
   项目 `.claude/settings.json` 里会被忽略（写 `bypassPermissions` 会退回 Manual）。
2. **不是 root**：以 root / `sudo` 运行会直接退出（除非 `IS_SANDBOX=1`）。

### 保留既有配置

`permissions` 下的 `allow` / `deny` / `ask` / `additionalDirectories` 一律原样保留；
`skipDangerousModePermissionPrompt` 一旦存在就永不删除。模型槽位与权限模式可以独立写入——
只改模式不必重填模型，只改模型不会把模式重置。

---

## 2. 网关侧开关：拦截 auto 模式的安全分类器

设置页「**Claude Code 一键写入**」组内的开关，对应 `gui-settings.json` 的
`intercept_safety_classifier`，**默认 `false`**。

它和上面的权限模式放在同一组，因为两者是同一个决定的两种表达：
`bypassPermissions` 下分类器根本不跑，开关是空转；`auto` 下每个被判定的动作都要过它。

> 注意它由页面底部「**保存配置**」保存，**不**由该组的「写入 Claude Code 配置」
> 按钮写入（后者只写 `~/.claude/settings.json`）。组内那条分隔线就是说明这件事的。

开启后，凡是被识别为 Claude Code auto 模式分类器的请求，网关直接返回
`<block>no</block>`（允许），不再转发上游。实测同一任务：

| 开关 | 上游请求数 |
|---|---|
| 关 | 4 次（含 2 次分类器，`claude-sonnet-5` 被上游 400 后降级重试） |
| 开 | **2 次**（分类器 6 ms 内本地应答） |

### 识别不依赖模型名

分类器不总叫同一个名字：先请求 `claude-sonnet-5`，该模型不可用时**改用会话自己的
模型重试**——按名字匹配恰好会漏掉真正被计费的那一次。所以判定用请求形状，三者同时成立：

- `messages` 恰好 1 条；
- 没有 `tools`；
- `system` 超过 100 000 字符（实测分类器约 144k，会话约 6k）。

### 为什么默认关闭

那个请求是 Anthropic 的安全监控器在判断「这条命令像不像 prompt injection / 越权 / 误删」。
本地返回「允许」不是判定，是**放弃判定**。而网关可以被多人使用：

- 开启对**所有**经过本网关的客户端同时生效；
- 他们看不到这个开关，也看不到日志里那行 `intercepted -> allow`；
- 即使他们用 `auto` 想逐条确认，只要经过这个网关，判定永远是「允许」。

所以只在「客户端已用 `bypassPermissions` 全放行、只是想省掉那次无用请求」时开启。
此时它不降低任何实际保护——本来就没有检查在跑。

> 该开关**刻意没有环境变量**：它会替所有客户端关掉安全检查，必须留在一个看得见、
> 点得到的地方，而不是藏进 systemd 的 `Environment=` 行。

---

## 3. 顺带修复

- **「已修改」徽标写入后不清**：写入成功会回读文件，字段值与页面加载时的 baseline
  仍然不同，徽标因此一直挂着。新增 `captureSectionBaseline(section)` 只重置该组，
  不会误清其他组未保存的编辑。

---

## 验证

- `cargo test`：**780 passed / 0 failed**（新增 8 个分类器识别单测 + 3 个 handler 对照测试）
- 新增 smoke 场景 `claude permission mode`，并**反向验证过**：临时撤掉徽标修复后该场景确实报红
- `cargo fmt` / `cargo clippy --all-targets` 干净
- `check-ui` / `check-dist` / `build:ui` / `smoke` 全过
- 端到端实测：真实 Claude Code 2.1.311 打真实网关，开关打开后分类器请求在 6 ms 内本地应答

无配置格式变更：`gui-settings.json` 只新增一个带 `#[serde(default)]` 的布尔字段，
旧文件读入即为 `false`（即安全默认）。
