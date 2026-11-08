# Claude Code 与上游网关拦截排查指南 (Code 11128)

本文档记录了在使用 Claude Code 通过本代理（`proxy-rs`）接入 WorkBuddy / 腾讯 Copilot 等上游服务商时，遇到上游错误码 `11128`（`Illegal API invocation from an unapproved channel` / `请求被安全策略拦截`）的根因排查过程、请求头分析及应对方案。

---

## 1. 错误现象

在终端使用 Claude Code 执行指令（例如通过 `claude -p "你好！"`）时，请求挂起或失败，代理日志（`~/.proxy-rs/logs/proxy.log` 及 GUI 客户端）中出现如下错误：

```text
2026-09-22 03:51:34.881 [INFO] POST /v1/messages model=deepseek-v4.1-flash -> upstream=deepseek-v4.1-flash stream=true msgs=2 tools=32
2026-09-22 03:51:35.371 [ERROR] UPSTREAM ERROR [chat/completions] url=https://copilot.tencent.com/v2/chat/completions model=deepseek-v4.1-flash 488ms | HTTP 400 · code 11128 Illegal API invocation from an unapproved channel · 请求被安全策略拦截，请稍后重试或联系支持。 · requestId 573b6989-3c54-47f8-921d-d66a7454446a | hint: 非法 API 调用：请求形态被上游拒绝 | body: {"code":11128,"msg":"Illegal API invocation from an unapproved channel","requestId":"573b6989-3c54-47f8-921d-d66a7454446a","displayMsg":{"en":"The request was blocked by security policy. Please retry later or contact support.","zh":"请求被安全策略拦截，请稍后重试或联系支持。","zh-hant":"請求已被安全策略攔截，請稍後重試或聯絡支援。"}}
2026-09-22 03:51:35.372 [ERROR] POST /v1/messages failed model=deepseek-v4.1-flash stream=true 492ms | Upstream API error: Upstream returned 400 Bad Request: {"code":11128,"msg":"Illegal API invocation from an unapproved channel",...}
```

随后若在 GUI 或系统托盘中暂停了代理，后续请求将直接返回：
```json
{"error":{"message":"Proxy service is paused from the console. Resume it to continue.","type":"service_unavailable"},"type":"error"}
```

---

## 2. Claude Code 实际发送的完整请求头

通过网关日志打印捕获，Claude Code（以 `2.1.228` 为例）向代理发送的完整请求头如下：

```http
accept: application/json
authorization: Bearer sk-***
content-type: application/json
user-agent: claude-cli/2.1.228 (external, cli)
x-claude-code-session-id: 46b168ec-dd90-4191-bd5a-5c51bd1d9470
x-app: cli
x-stainless-arch: arm64
x-stainless-lang: js
x-stainless-os: MacOS
x-stainless-package-version: 0.112.1
x-stainless-retry-count: 0
x-stainless-runtime: node
x-stainless-runtime-version: v26.3.0
x-stainless-timeout: 600
anthropic-version: 2023-06-01
anthropic-beta: claude-code-20250219,interleaved-thinking-2025-05-14,redact-thinking-2026-02-12,thinking-token-count-2026-05-13,context-management-2025-06-27,prompt-caching-scope-2026-01-05,mid-conversation-system-2026-04-07,advisor-tool-2026-03-01,effort-2025-11-24
anthropic-dangerous-direct-browser-access: true
connection: keep-alive
host: 127.0.0.1:3457
accept-encoding: gzip, deflate, br, zstd
```

---

## 3. 验证实验：请求头是否会导致拦截？

**结论：请求头完全不会触发拦截。**

### 验证命令
使用 `curl` 携带全套 Claude Code 请求头，但只发送一个简单的基础请求体（不包含 Claude Code 专有的长 System Prompt 和 32 个 Tools）。先把密钥放进环境变量，避免写进 shell 历史与终端回显：

```bash
export ANTHROPIC_API_KEY='sk-...'
```

```bash
curl -i -s -m 25 -X POST http://127.0.0.1:3457/v1/messages \
  -H 'accept: application/json' \
  -H "authorization: Bearer $ANTHROPIC_API_KEY" \
  -H 'content-type: application/json' \
  -H 'user-agent: claude-cli/2.1.228 (external, cli)' \
  -H 'x-claude-code-session-id: 46b168ec-dd90-4191-bd5a-5c51bd1d9470' \
  -H 'x-app: cli' \
  -H 'x-stainless-arch: arm64' \
  -H 'x-stainless-lang: js' \
  -H 'x-stainless-os: MacOS' \
  -H 'x-stainless-package-version: 0.112.1' \
  -H 'x-stainless-retry-count: 0' \
  -H 'x-stainless-runtime: node' \
  -H 'x-stainless-runtime-version: v26.3.0' \
  -H 'x-stainless-timeout: 600' \
  -H 'anthropic-version: 2023-06-01' \
  -H 'anthropic-beta: claude-code-20250219,interleaved-thinking-2025-05-14,redact-thinking-2026-02-12,thinking-token-count-2026-05-13,context-management-2025-06-27,prompt-caching-scope-2026-01-05,mid-conversation-system-2026-04-07,advisor-tool-2026-03-01,effort-2025-11-24' \
  -H 'anthropic-dangerous-direct-browser-access: true' \
  -d '{"model":"deepseek-v4.1-flash","max_tokens":64,"stream":true,"messages":[{"role":"user","content":"hi"}]}'
```

### 验证结果
```http
HTTP/1.1 200 OK
content-type: text/event-stream
...
event: message_start
data: {"type":"message_start","message":{"id":"cmb-...","type":"message","role":"assistant","model":"deepseek-v4.1-flash"}}
...
event: content_block_delta
data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hi! How's it going?"}}
...
event: message_stop
data: {"type":"message_stop"}
```
响应正常，耗时仅 800ms 左右。

---

## 4. 根因剖析：为什么 Claude Code 真实调用会触发 11128？

上游网关返回的 `11128` 是由 **请求体（Payload）** 触发的，主要包含以下触发点：

### 1) System Prompt 特征词触发 WAF 安全拦截（最主要原因）
Claude Code 在发请求时，默认会注入一段长达数千字的 System Prompt，包含了如：
`"You are Claude Code, Anthropic's official CLI..."` 等身份定义及特定工程指令。
- **验证发现**：直接向上游发送包含特定官方身份短语的 System Prompt，上游直接返回：
  `{"code":11128,"msg":"Illegal API invocation from an unapproved channel","displayMsg":{"zh":"请求被安全策略拦截，请稍后重试或联系支持。"}}`。
- 上游（WorkBuddy / 腾讯网关）配置了特定内容安全策略（WAF），会对 System Prompt 进行关键词审查。

### 2) 复杂的大规模工具定义（Tools 30+）
Claude Code 默认会把内置的所有工具（Bash, Edit, Read, Write, Glob, Grep, NotebookEdit, LSP 等 32 个工具）全部序列化并放入 `tools` 字段中。部分上游模型或网关参数校验器在解析此类未适配的复杂工具 schema 时，容易触发参数安全过滤拦截。

### 3) 非流式请求不支持
如果客户端没有开启 `stream: true`（比如测试时手动把 `stream` 改为 `false`），上游会返回：
`11101: Non-stream chat request is currently not supported`。

---

## 5. 应对与解决方案

### 1. 利用网关敏感词过滤功能
本代理提供了系统提示词过滤机制，可在转发给上游前自动剥离敏感关键词：
- 命令行参数：`--system-prompt-ignore "<敏感词1>;<敏感词2>"`
- 环境变量：`ANTHROPIC_PROXY_SYSTEM_PROMPT_IGNORE_TERMS="<敏感词1>;<敏感词2>"`

### 2. Claude Code 运行参数调整
- **自定义 System Prompt**：
  启动 Claude Code 时通过 `--system-prompt` 指定精炼的开发提示词，避免使用默认的官方长提示词：
  ```bash
  claude --system-prompt "You are a helpful coding assistant."
  ```
- **精简工具集**：
  通过 `--tools` 参数减少注入的工具数量（例如只启用必要的读写工具）：
  ```bash
  claude --tools "Bash,Edit,Read"
  ```
- **安全排障模式**：
  遇到异常时可加 `--safe-mode` 启动排查：
  ```bash
  claude --safe-mode
  ```

### 3. 网关日志监控与状态检查
- 请求头日志已内置：代理在收到所有 `/v1/messages` 和 `/v1/models` 请求时，均会在 `~/.proxy-rs/logs/proxy.log` 中记录 `POST /v1/messages headers: ...`，方便随时比对不同客户端的 Header。
- 若遇到 `503 Service Unavailable: Proxy service is paused from the console`，说明服务处于暂停状态，在 macOS 顶部状态栏托盘或桌面 GUI 客户端中点击“恢复/启动代理”即可。

---

## 6. 上游模型校验拦截：Code 11133 与 11148（工具历史形态不合规）

### 1) 现象与特征
在 Claude Code 会话历史中存在并行工具调用（tool_use）时，使用 `minimax-m3.1-flash` 或 `glm-5.3-flashx` 等严格模型节点常返回 HTTP 400：
```text
UPSTREAM ERROR ... HTTP 400 · code 11133 the request parameters were rejected by the model provider
// 或
UPSTREAM ERROR ... HTTP 400 · code 11148 tool calls and tool results do not match
```
而在同样会话下换用 `hy4free` / `deepseek-v4.1-flash` 却能正常工作，且新建空会话也能正常工作。

### 2) 根因剖析
- **区分 11128 与 11133**：`11128` 是网关 WAF 安全策略/敏感词拦截；`11133` / `11148` 是**模型提供方对 OpenAI 消息序列与工具调用闭环规范的严格校验拦截**。
- Claude Code 的 Anthropic 会话结构中，并发工具调用常拆分为多个消息块，甚至连续出现多个 `assistant` 消息（例如纯文本前导说明后跟工具调用）。直接按 1:1 翻译会产生连续的 `assistant` 消息，以及 `tool_result` 顺序与 `tool_calls` 不一致的情况。
- 此外，Claude Code 在特定异常重试后，可能会在历史中残留 `id` 为空或函数名为空的伪 tool 调用。
- 宽松节点对上述畸形格式予以宽容，而严格模型节点会判定参数非法并拒收。

### 3) 代理内置解决方案（消息序列归一化器）
代理在翻译管道中内置了纯函数 `normalize_message_sequence`：
1. **合并相邻连续 assistant 消息**：自动拼接文本内容与 `reasoning_content`，合并并发 `tool_calls`，消除连续同角色助手消息。
2. **过滤畸形伪工具调用**：剥离 `id` 为空或 `name` 为空的无效调用，防止触犯函数命名规范。
3. **严格重排与闭环 tool 响应**：按当前助手消息中 `tool_calls` 的声明顺序重排后续的 `tool` 结果消息；若存在缺失结果自动补齐 `[result omitted]` 占位，若存在孤儿结果安全丢弃，确保完整闭环。
4. **确定性保缓存**：整个归一化算法为无副作用纯函数，历史前缀归一化后字节完全稳定，不会破坏上游 Prompt Cache。

---

## 7. 让 Claude Code 不再逐条询问工具权限（以及「分类器开销」到底是什么）

### 1) 问题：权限确认有两种，代价完全不同

Claude Code 在「要不要执行这个工具调用」上有两套机制，混为一谈会得出错误结论：

| 机制 | 命令行 | 每次工具调用的代价 |
|---|---|---|
| **询问用户** | `default` / `manual` | 人肉等待，非交互（`-p`）场景直接拒绝 |
| **分类器判定** | `auto` | **多一次 API 请求**（见下），或走服务端不另计费的新路径 |
| **全部放行** | `bypassPermissions` | **零额外请求** |

`auto` 模式的判定不是本地的：Claude Code 会把「精简后的对话 + 待执行动作」打包成一次独立请求发给判定模型。第三方实测（分类器继承主模型时）约占整个会话 token 的 15–28%；后来分类器固定用小模型，账单冲击变小，但**请求次数**没变——一次工具调用就多一次往返。

本机实测（同一任务：连续 3 次 `Write`，Claude Code 2.1.311，经本网关计数）：

| `permissions.defaultMode` | 上游请求数 | 说明 |
|---|---|---|
| `bypassPermissions` | **4** | 1 次对话 + 3 次工具结果回灌，无判定请求 |
| `auto` | **7** | 1 + 3，外加 **3 次判定请求**（输出仅 4 token，输入可达 3 万+ token） |

> 判定请求是「按被检查的动作」计，不是按工具调用计：已经被 allow 规则覆盖、或在 `acceptEdits` 语义下本就自动通过的动作（例如在工作目录内 `Write`）会**跳过**判定。所以上面 3 次是这次任务里真正需要判定次数，不是上限。

### 2) v2.1.278 的「不再收费」改了什么——以及本网关为什么吃不到

Anthropic 的做法是把安全检查塞进**会话本身的那次请求**：请求体多一个顶层 `safeguards` 字段（配对一个 `anthropic-beta` 值），服务端把结论放在响应的顶层 `safeguard_results` 里。检查与正常对话共用一次调用，因此不另计费。

实测抓到的请求形状（`auto` 模式，经抓包网关转发）：

```json
{
  "model": "…",
  "messages": [ … ],
  "tools": [ … ],
  "safeguards": [
    {
      "type": "dangerous_tool_use",
      "classifier_context": {
        "v": 1,
        "permission_mode": "auto",
        "platform": "macos",
        "live_cwd": "/private/tmp",
        "trusted_directories": { … },
        "rules": { "allow": [], "deny": [], "ask": [] },
        "auto_mode": { "allow": [], "soft_deny": [], "hard_deny": [], "environment": [] },
        "git_state": { … }
      }
    }
  ]
}
```

**关键限制：这条路径要求上游真的会判定。** 本网关的上游是 OpenAI 兼容端点（WorkBuddy / copilot.tencent.com），它不认识 `safeguards`，更不会返回 `safeguard_results`。Claude Code 的行为是明确的：

```text
[server-classifier] the API answered that it does not run the server-side classifier
for this session (unsupported), so auto mode classifies locally for the rest of this session
```

并在交互/`-p` 会话里打印：

```text
We're changing auto mode to no longer charge for classifier requests in Claude Code.
However, this session isn't eligible because your requests go through 127.0.0.1:3457,
which isn't compatible with this update.
```

也就是说：**判定请求照发、照计费**，只是换成了 Claude Code 自己发。把 `safeguards` 原样透传给一个不认识它的上游，不会让它变得更便宜；而**伪造** `safeguard_results`（把每个动作标成 `not_flagged`）技术上做得到，但那是在冒充 Anthropic 的安全判定，本代理不做——这是一条策略红线，不是能力限制。

### 3) 正确做法：直接不要判定

如果目标是「不再有任何工具确认」，那就不要待在 `auto` 里。写两个键即可，**零额外请求**：

```json
{
  "permissions": { "defaultMode": "bypassPermissions" },
  "skipDangerousModePermissionPrompt": true
}
```

同样是 `~/.claude/settings.json`，现在设置页「Claude Code 一键写入」里可以直接选：

- 位置：`permissions.defaultMode`
- **必须一起写** `skipDangerousModePermissionPrompt: true`。这不是绕过同意书：`bypassPermissions` 在 CLI 侧被一个**同意记录**门控着——没有它，CLI 首次进入该模式要弹窗确认，而后台 / `--bg` 会话会被**静默降级回 `default`**（表现为「点了没生效、还是在问」）。手动接受那个弹窗写的就是同一个键，所以一键写入等于替用户记录这份同意，而不是跳过它。

两个容易踩的坑，实测确认：

1. **作用域**：`auto` 与 `bypassPermissions` 只在**用户设置**（`~/.claude/settings.json`）或 managed 策略/`--permission-mode` 下生效；自 v2.1.257 起，写在项目 `.claude/settings.json` / `.claude/settings.local.json` 里会被**忽略**（写 `bypassPermissions` 会退回 Manual）。本代理写的就是用户文件，所以是对的；手工改配置时别改错文件。
2. **不是 root**：Linux/macOS 上以 root / `sudo` 运行会直接退出并打印 `--dangerously-skip-permissions cannot be used with root/sudo privileges for security reasons`（除非 `IS_SANDBOX=1` 或 bubblewrap 沙箱）。容器里以 root 跑 CI 的话要显式设 `IS_SANDBOX=1`。

### 4) 连 `bypassPermissions` 都拦不住的四类动作

即使开了全放行，下列动作仍会询问或拒绝，属于设计如此，不是配置错了：

- 用户显式写的 `deny` / `ask` 规则（deny 在所有模式生效，`ask` 在 `auto` 下仍弹窗；`bypassPermissions` 下 allow 规则失效，但 deny 仍生效）
- `AskUserQuestion` 这类「必须由人回答」的工具
- 识别为关键路径的 `rm` / `rmdir`（如 `rm -rf /`、`rm -rf ~`），默认 2 分钟倒计时后拒绝
- `permissions.blockReadsOutsideWorkingDirectories` 开启时的目录外读取

### 5) 各模式一览（写入 `permissions.defaultMode`）

| 值 | 行为 | 额外请求 |
|---|---|---|
| `default`（别名 `manual`） | 每次都询问 | 无 |
| `acceptEdits` | 自动接受工作目录内的文件编辑与常见文件命令（`mkdir`/`mv`/…），其余仍询问 | 无 |
| `plan` | 只读规划，不改文件 | 视配置 |
| `auto` | 分类器逐条判定 | **有**（本网关上为计费请求） |
| `dontAsk` | 不会询问：凡是会弹窗的一律**拒绝** | 无 |
| `bypassPermissions` | 全部自动通过 | **无** |

> `dontAsk` 与 `bypassPermissions` 方向相反，别选错：前者是「不问我，一律拒绝」，后者是「不问我，一律通过」。

### 6) 网关侧开关：拦截分类器并直接放行

设置页「**Claude Code 一键写入**」组里的开关：**拦截 auto 模式的安全分类器（直接放行）**，对应 `gui-settings.json` 的 `intercept_safety_classifier`，**默认 `false`**。

它和权限模式放在同一组，因为两者是同一个决定的两种表达：`bypassPermissions` 下分类器根本不跑（开关空转），`auto` 下每个被判定的动作都要过它。

> **由页面底部「保存配置」保存**，不由该组的「写入 Claude Code 配置」按钮写入 —— 后者只写 `~/.claude/settings.json`。组内那条分隔线就是说明这件事的。

开启后，凡是被识别为「Claude Code auto 模式分类器」的请求，网关直接返回 `<block>no</block>`（允许），不再转发上游。实测（2.1.311 + 本网关）：

| 开关 | 上游请求数 | 日志 |
|---|---|---|
| 关 | 4（含 2 次分类器请求，其中 `claude-sonnet-5` 被上游 400 后降级重试） | 无特殊记录 |
| 开 | 2（分类器在 6ms 内本地应答） | `auto-mode classifier intercepted -> allow (model=claude-sonnet-5, 6ms)` |

**识别方式不依赖模型名**，因为分类器不总叫同一个名字：先请求 `claude-sonnet-5`，该模型不可用时**改用会话自己的模型重试**（实测确认）。按名字匹配恰好会漏掉真正被计费的那一次。所以判定用请求形状，三者同时成立：

- `messages` 恰好 1 条（被判定的不是对话，是一段待判 transcript）
- 没有 `tools`（分类器是纯文本判官，真实 agent turn 必带工具列表）
- `system` 超过 100 000 字符（实测分类器约 144k，Claude Code 会话约 6k）

任一条单看都是猜测，三条合起来在本代理见过的请求里没有别的形态。阈值取得很松（会话提示词要涨 16 倍才撞线），但两条结构字段是精确的。

**回应格式**：Claude Code 解析 `<block>yes|no</block>`；`yes` 拦截、`no` 允许、**完全没有 `<block>` 标签也算允许**。网关显式发 `no`，不依赖那个兜底。

**为什么默认关闭，以及开启意味着什么**

那个请求是 Anthropic 的安全监控器在判断「这条命令像不像 prompt injection / 越权 / 误删」。本地返回「允许」不是判定，是**放弃判定**。而本网关可以被多人使用：

- 开启对**所有**经过本网关的客户端同时生效，包括别人；
- 他们看不到这个开关，也看不到日志里那行 `intercepted -> allow`；
- 即使他们用 `default` / `auto` 想逐条确认，只要经过这个网关，判定永远是「允许」。

所以只在「客户端已经用 `bypassPermissions` 全放行、��是想省掉那次无用请求」时开启。此时它不降低任何实际保护——本来就没有检查在跑。

> 反过来说：如果网关上有**任何**客户端依赖 `auto` 模式来做真实的安全判定，这个开关打开就是替他们关掉了那道检查。
